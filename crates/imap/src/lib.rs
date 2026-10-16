// SPDX-License-Identifier: AGPL-3.0-only

//! IMAP backend: implements [`MailSource`] over one account.

use std::fmt;
use std::sync::Arc;

use async_imap::types::{Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use beavermail_core::{AuthCheck, AuthResult, Account, Attachment, Hit, Keep, MailError, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Progress, Role, Server, SyncReport, Tls};
use beavermail_store::{Header, Location, Store};
use beavermail_vault::{Vault, folder_path, message_path};
use futures::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
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
const HEADER_FIELDS: &str = "BODY.PEEK[HEADER.FIELDS (FROM TO CC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE AUTHENTICATION-RESULTS X-SPAM-STATUS X-SPAM-FLAG X-SPAM-BAR X-SPAM-SCORE X-SPAM-REPORT)]";

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
    /// To the folder with this role (Spam, or back to the Inbox).
    Move(Role),
    /// Permanent delete; allowed only inside the trash folder.
    Erase,
}

pub struct ImapSource {
    config: Config,
    /// How often a server without IDLE is asked whether anything changed.
    poll_interval: std::time::Duration,
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
    /// The user chose to work offline: no connection is opened, and an open one is dropped.
    offline: std::sync::atomic::AtomicBool,
    /// Signed in with a provider: an access token instead of the password.
    token: Option<beavermail_core::TokenSource>,
}

impl ImapSource {
    /// Working offline: every call that needs the server answers [`MailError::Offline`] at once, and
    /// the archive keeps answering the rest.
    pub fn set_offline(&self, offline: bool) {
        self.config.offline.store(offline, std::sync::atomic::Ordering::Relaxed);
    }

    /// The message as stored on this computer, without going to the server; `None` when it is not stored.
    pub fn stored_body(&self, id: MessageId) -> Result<Option<MessageBody>, MailError> {
        let at = self.store.locate(id)?.ok_or(MailError::NotFound)?;
        let path = self.file_of(&at.account, &at.folder, at.uid)?;
        self.files.read(&path)?.map(|raw| parse_body(&raw)).transpose()
    }

    /// What the kept header fields say (sender, Reply-To, the server's checks), without text or
    /// attachments; `None` when they were not read yet.
    pub fn header_body(&self, id: MessageId) -> Result<Option<MessageBody>, MailError> {
        self.store.headers(id)?.filter(|raw| !raw.is_empty()).map(|raw| parse_body(&raw)).transpose()
    }

    /// Creates the source over the shared archive; the connection is opened on first use.
    pub fn new(account: &Account, password: String, store: Arc<Store>, files: Arc<Vault>) -> Self {
        let config = Config { server: account.imap.clone(), user: account.user.clone(), password, offline: Default::default(), token: None };
        Self { config, poll_interval: std::time::Duration::from_secs(180), account: account.key(), store, files, state: Mutex::new(State::default()) }
    }

    /// Changes how often a server without IDLE is polled (3 minutes by default).
    #[must_use]
    /// Signs in with `token`'s access tokens (XOAUTH2) instead of the password.
    pub fn with_token(mut self, token: beavermail_core::TokenSource) -> Self {
        self.config.token = Some(token);
        self
    }

    pub fn with_poll_interval(mut self, interval: std::time::Duration) -> Self {
        self.poll_interval = interval;
        self
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
    if c.offline.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(MailError::Offline("working offline".into()));
    }
    let Server { host, port, tls: mode } = &c.server;
    let tcp = TcpStream::connect((host.as_str(), *port)).await.map_err(|e| MailError::Offline(e.to_string()))?;
    let mut client = match mode {
        Tls::None => Client::new(Box::new(tcp) as Stream),
        Tls::Tls => Client::new(tls(tcp, host).await?),
        Tls::StartTls => {
            let mut plain = Client::new(tcp);
            plain.read_response().await.map_err(backend)?; // server greeting
            plain.run_command_and_check_ok("STARTTLS", None).await.map_err(backend)?;
            Client::new(tls(plain.into_inner(), host).await?)
        }
    };
    if *mode != Tls::StartTls {
        // LOGIN skips the greeting, but AUTHENTICATE would take it for the server's challenge and wait.
        client.read_response().await.map_err(backend)?;
    }
    match &c.token {
        // Asked off the async threads: getting a token may ask the provider for a new one.
        Some(token) => {
            let token = token.clone();
            let access = tokio::task::spawn_blocking(move || token()).await.map_err(backend)??;
            client.authenticate("XOAUTH2", XOAuth2 { user: c.user.clone(), token: access, answered: false }).await.map_err(|(e, _)| MailError::Auth(e.to_string()))
        }
        None => client.login(&c.user, &c.password).await.map_err(|(e, _)| MailError::Auth(e.to_string())),
    }
}

/// The XOAUTH2 answer to the server's challenge. If the server refuses the token it sends a second
/// challenge with the reason, which an empty answer closes, so the refusal comes back as an error.
struct XOAuth2 {
    user: String,
    token: String,
    answered: bool,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> String {
        if std::mem::replace(&mut self.answered, true) {
            return String::new();
        }
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

impl State {
    /// The open session, connecting first if needed.
    async fn session(&mut self, config: &Config) -> Result<&mut Session<Stream>, MailError> {
        if config.offline.load(std::sync::atomic::Ordering::Relaxed) {
            self.session = None; // closed, so nothing more goes over it
            return Err(MailError::Offline("working offline".into()));
        }
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

fn has_param(params: &async_imap::imap_proto::BodyParams<'_>, name: &str) -> bool {
    params.as_ref().is_some_and(|list| list.iter().any(|(key, _)| key.eq_ignore_ascii_case(name)))
}

/// Whether the message structure has a file attached: a part marked as an attachment, or a named
/// one that is not embedded in the text (embedded parts have a Content-ID).
fn carries_file(structure: &async_imap::imap_proto::BodyStructure<'_>) -> bool {
    use async_imap::imap_proto::BodyStructure::{Basic, Message, Multipart, Text};
    match structure {
        Multipart { bodies, .. } => bodies.iter().any(carries_file),
        Basic { common, other, .. } | Text { common, other, .. } | Message { common, other, .. } => {
            let disposition = common.disposition.as_ref();
            let attached = disposition.is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"));
            let named = disposition.is_some_and(|d| has_param(&d.params, "filename")) || has_param(&common.ty.params, "name");
            attached || (named && other.id.is_none())
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
        size: fetch.size.unwrap_or(0),
        recipients: recipients(&message),
        raw: raw.to_vec(),
        // The server's description of the message is exact; the Content-Type is a guess for servers
        // that do not send it.
        has_attachments: fetch.bodystructure().map_or_else(
            || message.root_part().content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("multipart") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed"))),
            carries_file,
        ),
    })
}

/// The To and Cc addresses, lower case, separated by spaces.
fn recipients(message: &mail_parser::Message<'_>) -> String {
    [message.to(), message.cc()].into_iter().flatten().flat_map(|h| h.iter()).filter_map(|a| a.address()).map(str::to_lowercase).collect::<Vec<_>>().join(" ")
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
            Op::Move(role) => Some(self.folder_with_role(role).await?.ok_or_else(|| backend(format!("this account has no {role:?} folder")))?),
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
            Op::Archive | Op::Trash | Op::Move(_) | Op::Erase => {
                self.drop_files(&at.folder, &[at.uid]);
                self.store.remove(id)
            }
        }
    }

    async fn fetch_headers(session: &mut Session<Stream>, set: &str, by_uid: bool) -> Result<Vec<Header>, async_imap::error::Error> {
        let query = format!("(UID FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE {HEADER_FIELDS})");
        let fetches: Vec<Fetch> = if by_uid {
            session.uid_fetch(set, &query).await?.try_collect().await?
        } else {
            session.fetch(set, &query).await?.try_collect().await?
        };
        Ok(fetches.iter().filter_map(header).collect())
    }

    /// Messages fetched together: enough to keep the connection busy, small enough that opening
    /// a message in the meantime does not wait long.
    const BATCH_MESSAGES: usize = 25;
    const BATCH_BYTES: u64 = 4 * 1024 * 1024;

    /// Makes the text of messages that were stored before searching existed (or whose download did not
    /// get to it) searchable. A message whose file turns out to be missing is marked as not stored, so
    /// the next download fetches it again.
    async fn index_stored(&self) -> Result<(), MailError> {
        loop {
            let batch = self.store.unindexed(&self.account, 200)?;
            if batch.is_empty() {
                return Ok(());
            }
            for (id, folder, uid) in batch {
                let path = self.file_of(&self.account, &folder, uid)?;
                match self.files.read(&path) {
                    Ok(Some(raw)) => match parse_body(&raw) {
                        Ok(body) => {
                            self.store.set_preview(id, &body.text)?;
                            self.store.set_attachments(&self.account, &folder, uid, !body.attachments.is_empty())?;
                            self.store.index_body(id, &body.text)?;
                        }
                        Err(_) => self.store.mark_indexed(id)?, // not a message: do not look again
                    },
                    Ok(None) => self.store.clear_stored(id)?,
                    Err(MailError::Auth(why)) => return Err(MailError::Auth(why)), // locked: try again after unlocking
                    Err(_) => self.store.mark_indexed(id)?,
                }
            }
            tokio::task::yield_now().await;
        }
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

        // Rows from before attachments were tracked: ask the server about a chunk of them.
        for _ in 0..20 {
            let unknown = store.unknown_attachments(account, folder, 300)?;
            if unknown.is_empty() {
                break;
            }
            let set = unknown.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let fetches: Vec<Fetch> = session.uid_fetch(&set, "(UID BODYSTRUCTURE)").await?.try_collect().await?;
            for fetch in &fetches {
                if let (Some(uid), Some(structure)) = (fetch.uid, fetch.bodystructure()) {
                    store.set_attachments(account, folder, uid, carries_file(structure))?;
                }
            }
            // The server no longer has the ones that did not answer; they are settled as "none".
            for uid in &unknown {
                if !fetches.iter().any(|f| f.uid == Some(*uid)) {
                    store.set_attachments(account, folder, *uid, false)?;
                }
            }
        }

        // Sent rows from before recipients were kept: whom the user wrote to decides who is a known sender.
        if store.folder_with_role(account, Role::Sent)?.as_deref() == Some(folder) {
            for _ in 0..20 {
                let unknown = store.unknown_recipients(account, folder, 300)?;
                if unknown.is_empty() {
                    break;
                }
                let set = unknown.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                let fetches: Vec<Fetch> = session.uid_fetch(&set, "(UID BODY.PEEK[HEADER.FIELDS (TO CC)])").await?.try_collect().await?;
                for uid in &unknown {
                    // One the server no longer has is settled as "nobody".
                    let found = fetches.iter().find(|f| f.uid == Some(*uid)).and_then(|f| f.header()).and_then(|raw| MessageParser::default().parse_headers(raw));
                    store.set_recipients(account, folder, *uid, &found.map(|m| recipients(&m)).unwrap_or_default())?;
                }
            }
        }

        // Inbox rows from before header fields were kept: the screener checks them without the body.
        if store.folder_with_role(account, Role::Inbox)?.as_deref() == Some(folder) {
            for _ in 0..20 {
                let unknown = store.unknown_headers(account, folder, 300)?;
                if unknown.is_empty() {
                    break;
                }
                let set = unknown.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                let fetches: Vec<Fetch> = session.uid_fetch(&set, format!("(UID {HEADER_FIELDS})")).await?.try_collect().await?;
                for uid in &unknown {
                    let raw = fetches.iter().find(|f| f.uid == Some(*uid)).and_then(|f| f.header()).unwrap_or_default();
                    store.set_headers(account, folder, *uid, raw)?;
                }
            }
        }

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

impl ImapSource {
    /// Downloads one batch of `pending` messages of `folder` into the vault. Returns how many arrived.
    async fn download_batch(&self, folder: &str, pending: &[beavermail_store::Pending]) -> Result<(), MailError> {
        let uids = pending.iter().map(|p| p.uid.to_string()).collect::<Vec<_>>().join(",");
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.examine(folder).await?;
            let fetches: Vec<Fetch> = session.uid_fetch(&uids, "(UID BODY.PEEK[])").await?.try_collect().await?;
            Ok(fetches)
        }
        .await;
        let fetches = result.map_err(|e| state.failed(e))?;
        drop(state);
        for fetch in &fetches {
            let (Some(uid), Some(raw)) = (fetch.uid, fetch.body()) else { continue };
            let path = self.file_of(&self.account, folder, uid)?;
            self.files.write(&path, raw)?;
            if let Some(id) = self.store.id_of(&self.account, folder, uid)?
                && let Ok(body) = parse_body(raw)
            {
                self.store.set_preview(id, &body.text)?;
                self.store.set_attachments(&self.account, folder, uid, !body.attachments.is_empty())?;
                self.store.index_body(id, &body.text)?;
            }
            self.store.mark_stored(&self.account, folder, uid)?;
        }
        Ok(())
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

/// The files attached to a message: parts that are not the text, named or marked as attachments.
/// Pictures embedded in the HTML (they have a `Content-ID` and no attachment marking) are not listed.
fn attachments(message: &mail_parser::Message<'_>) -> Vec<Attachment> {
    message
        .attachments()
        .enumerate()
        .filter(|(_, part)| {
            let marked = part.content_disposition().is_some_and(|d| d.ctype().eq_ignore_ascii_case("attachment"));
            marked || (part.attachment_name().is_some() && part.content_id().is_none())
        })
        .map(|(index, part)| Attachment {
            index: index as u32,
            name: part.attachment_name().map_or_else(|| format!("attachment-{}", index + 1), str::to_owned),
            mime: part.content_type().map_or_else(|| "application/octet-stream".to_owned(), |c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream"))),
            size: part.contents().len() as u64,
        })
        .collect()
}

/// The name and decoded bytes of attachment number `index` of a complete message.
fn attachment_data(raw: &[u8], index: u32) -> Result<(String, Vec<u8>), MailError> {
    let message = MessageParser::default().parse(raw).ok_or(MailError::NotFound)?;
    let part = message.attachment(index).ok_or(MailError::NotFound)?;
    Ok((part.attachment_name().map_or_else(|| format!("attachment-{}", index + 1), str::to_owned), part.contents().to_vec()))
}

/// The text, recipients and threading headers of a complete message.
fn parse_body(raw: &[u8]) -> Result<MessageBody, MailError> {
    let message = MessageParser::default().parse(raw).ok_or(MailError::NotFound)?;
    Ok(MessageBody {
        attachments: attachments(&message),
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
        auth: trace_headers(raw).iter().filter(|(n, _)| n == "authentication-results").map(|(_, v)| auth_result(v)).collect(),
        spam: trace_headers(raw).iter().any(|(n, v)| spam_verdict(n, v)),
        spam_tests: trace_headers(raw).iter().flat_map(|(n, v)| spam_tests(n, v)).collect(),
        unsubscribe: trace_headers(raw).iter().filter(|(n, _)| n == "list-unsubscribe").flat_map(|(_, v)| unsubscribe_links(v)).collect(),
        one_click: trace_headers(raw).iter().any(|(n, v)| n == "list-unsubscribe-post" && v.replace(' ', "").eq_ignore_ascii_case("List-Unsubscribe=One-Click")),
    })
}

/// The `<…>` links of a `List-Unsubscribe` value that a mail client may use: `https:` and `mailto:`.
fn unsubscribe_links(value: &str) -> Vec<String> {
    value
        .split('<')
        .skip(1)
        .filter_map(|part| part.split_once('>'))
        .map(|(link, _)| link.split_whitespace().collect::<String>())
        .filter(|link| ["https:", "mailto:"].iter().any(|s| link.get(..s.len()).is_some_and(|p| p.eq_ignore_ascii_case(s))))
        .collect()
}

/// The header lines of a message as (lower-case name, unfolded value), in order.
fn trace_headers(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let end = text.find("\r\n\r\n").or_else(|| text.find("\n\n")).unwrap_or(text.len());
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text[..end].lines() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    headers
}

/// A spam filter's header that says the message is spam: SpamAssassin's `X-Spam-Status: Yes` and
/// `X-Spam-Flag: YES`, or Exim's `X-Spam-Bar` with five or more `+` (one per point, the usual threshold).
fn spam_verdict(name: &str, value: &str) -> bool {
    match name {
        "x-spam-status" => value.trim_start().to_ascii_lowercase().starts_with("yes"),
        "x-spam-flag" => value.trim().eq_ignore_ascii_case("yes"),
        "x-spam-bar" => value.chars().filter(|c| *c == '+').count() >= 5,
        _ => false,
    }
}

/// The rule names a SpamAssassin header reports: `tests=A,B` in `X-Spam-Status`, or the "points
/// RULE_NAME" lines of `X-Spam-Report`.
fn spam_tests(name: &str, value: &str) -> Vec<String> {
    let rule = |w: &str| w.len() >= 3 && w.contains('_') && w.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    match name {
        "x-spam-status" => value.split_once("tests=").map(|(_, t)| t.split_whitespace().next().unwrap_or("").split(',').filter(|w| rule(w)).map(str::to_owned).collect()).unwrap_or_default(),
        "x-spam-report" => {
            let words: Vec<&str> = value.split_whitespace().collect();
            words.windows(2).filter(|w| w[0].parse::<f32>().is_ok() && rule(w[1])).map(|w| w[1].to_owned()).collect()
        }
        _ => Vec::new(),
    }
}

/// An `Authentication-Results` value: "server; spf=pass smtp.mailfrom=a@b; dkim=fail header.d=b".
fn auth_result(value: &str) -> AuthResult {
    let mut parts = value.split(';');
    let server = parts.next().unwrap_or("").split_whitespace().next().unwrap_or("").trim_end_matches('.').to_lowercase();
    let checks = parts
        .filter_map(|part| {
            let mut words = part.split_whitespace();
            let (method, result) = words.next()?.split_once('=')?;
            // The domain the check is about: the signer of a DKIM signature, the From domain of DMARC, the envelope sender of SPF.
            let domain = words
                .filter_map(|w| w.split_once('='))
                .find_map(|(k, v)| match (k.to_ascii_lowercase().as_str(), method.to_ascii_lowercase().as_str()) {
                    ("header.d", "dkim") | ("header.from", "dmarc") | ("smtp.mailfrom", "spf") | ("smtp.helo", "spf") => Some(v),
                    _ => None,
                })
                .map(|v| v.trim_matches(['"', '<', '>']).rsplit('@').next().unwrap_or("").to_lowercase())
                .unwrap_or_default();
            Some(AuthCheck { method: method.to_lowercase(), result: result.trim_matches(['(', ')']).to_lowercase(), domain })
        })
        .collect();
    AuthResult { server, checks }
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
        Ok(self.store.folders(&self.account)?.into_iter().map(|f| Mailbox { name: f.name, unread: f.unread, role: f.role, total: f.total, stored: f.stored }).collect())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, MailError> {
        self.store.search(&self.account, query, limit)
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
                let status = session.status(name, "(UNSEEN MESSAGES)").await.map_err(backend)?;
                mailboxes.push(Mailbox { name: name.clone(), unread: status.unseen.unwrap_or(0), role: *role, total: status.exists, stored: 0 });
            }
            Ok((folders, mailboxes))
        }
        .await;
        match result {
            Ok((folders, mut mailboxes)) => {
                self.store.set_folders(&self.account, &folders)?;
                for m in &mut mailboxes {
                    self.store.set_total(&self.account, &m.name, m.total)?;
                }
                // The counts of what is stored come from the archive, the rest from the server.
                for row in self.store.folders(&self.account)? {
                    if let Some(m) = mailboxes.iter_mut().find(|m| m.name == row.name) {
                        m.stored = row.stored;
                    }
                }
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

    async fn watch(&self, mailbox: &str, changed: &(dyn Fn() + Send + Sync), stop: &std::sync::atomic::AtomicBool) -> Result<(), MailError> {
        use std::sync::atomic::Ordering::Relaxed;
        use std::time::Duration;
        // Its own connection: IDLE keeps one busy for as long as it runs.
        let mut session = connect(&self.config).await?;
        let idle_supported = session.capabilities().await.map_err(backend)?.has_str("IDLE");
        session.examine(mailbox).await.map_err(backend)?;
        // Some networks drop quiet connections after five minutes; IDLE is renewed before that.
        let renew = Duration::from_secs(4 * 60);
        while !stop.load(Relaxed) {
            if !idle_supported {
                // No IDLE: wait, then ask, and tell the caller to look again.
                let mut waited = Duration::ZERO;
                while waited < self.poll_interval && !stop.load(Relaxed) {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    waited += Duration::from_millis(250);
                }
                if !stop.load(Relaxed) {
                    session.noop().await.map_err(backend)?;
                    changed();
                }
                continue;
            }
            let mut idle = session.idle();
            idle.init().await.map_err(backend)?;
            // The waiting future borrows `idle`; it ends with this block so that `done()` can take it.
            let response = {
                let (waiting, interrupt) = idle.wait_with_timeout(renew);
                tokio::pin!(waiting);
                let mut interrupt = Some(interrupt);
                loop {
                    tokio::select! {
                        response = &mut waiting => break response.map_err(backend)?,
                        () = tokio::time::sleep(Duration::from_millis(250)) => {
                            if stop.load(Relaxed) {
                                interrupt.take(); // dropping the source ends the wait
                            }
                        }
                    }
                }
            };
            if matches!(response, async_imap::extensions::idle::IdleResponse::NewData(_)) {
                changed();
            }
            session = idle.done().await.map_err(backend)?;
        }
        let _ = session.logout().await;
        Ok(())
    }

    async fn download(&self, keep: Keep, progress: &Progress) -> Result<(), MailError> {
        use std::sync::atomic::Ordering::Relaxed;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        // Searching the text needs it indexed, whatever is kept offline.
        self.index_stored().await?;
        let Some(since) = keep.since(now) else { return Ok(()) };
        // The Spam folder's headers, not its bodies: what the server set aside teaches the spam checks.
        if let Some(junk) = self.store.folder_with_role(&self.account, Role::Junk)? {
            progress.set_activity(format!("Checking {junk}"));
            self.sync_folder(&junk).await?;
        }
        let mut folders: Vec<String> = self.store.folders(&self.account)?.into_iter().filter(|f| !matches!(f.role, Role::Junk | Role::Trash)).map(|f| f.name).collect();
        folders.sort_by_key(|n| n != "INBOX");

        // The folders must be known far enough back before there is anything to count.
        for folder in &folders {
            progress.set_activity(format!("Checking {folder}"));
            loop {
                if progress.cancel.load(Relaxed) {
                    return Ok(());
                }
                let report = self.sync_folder(folder).await?;
                let oldest = self.store.oldest_timestamp(&self.account, folder)?;
                if report.remaining == 0 || oldest.is_some_and(|t| t < since) {
                    break;
                }
            }
        }
        let mut total = 0;
        for folder in &folders {
            total += self.store.count_pending(&self.account, folder, since)?;
        }
        progress.total.store(total, Relaxed);
        progress.done.store(0, Relaxed);

        for folder in &folders {
            progress.set_activity(format!("Downloading {folder}"));
            loop {
                while progress.paused.load(Relaxed) && !progress.cancel.load(Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
                if progress.cancel.load(Relaxed) {
                    return Ok(());
                }
                let pending = self.store.pending(&self.account, folder, since, Self::BATCH_MESSAGES)?;
                // A batch ends at the byte limit; a single large message is a batch of its own.
                let mut bytes = 0u64;
                let batch: Vec<_> = pending
                    .iter()
                    .take_while(|p| {
                        let fits = bytes == 0 || bytes + u64::from(p.size) <= Self::BATCH_BYTES;
                        bytes += u64::from(p.size);
                        fits
                    })
                    .copied()
                    .collect();
                if batch.is_empty() {
                    break;
                }
                self.download_batch(folder, &batch).await?;
                // A message the server no longer has (deleted meanwhile) must not be asked for forever;
                // the next sync removes it from the list.
                for p in &batch {
                    self.store.mark_stored(&self.account, folder, p.uid)?;
                }
                progress.done.fetch_add(batch.len() as u32, Relaxed);
            }
        }
        progress.set_activity("");
        Ok(())
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
            self.store.mark_stored(&at.account, &at.folder, at.uid)?;
            self.store.set_attachments(&at.account, &at.folder, at.uid, !body.attachments.is_empty())?;
            self.store.index_body(id, &body.text)?;
        }
        Ok(body)
    }

    async fn attachment(&self, id: MessageId, index: u32) -> Result<(String, Vec<u8>), MailError> {
        let at = self.store.locate(id)?.ok_or(MailError::NotFound)?;
        let path = self.file_of(&at.account, &at.folder, at.uid)?;
        let raw = match self.files.read(&path).ok().flatten() {
            Some(raw) => raw,
            None => {
                let raw = self.download(&at).await?;
                // As for the body: failing to keep the file does not stop the attachment from opening.
                if self.files.write(&path, &raw).is_ok() {
                    self.store.mark_stored(&at.account, &at.folder, at.uid)?;
                }
                raw
            }
        };
        attachment_data(&raw, index)
    }

    async fn save_draft(&self, id: &str, raw: Vec<u8>) -> Result<(), MailError> {
        let folder = self.folder_with_role(Role::Drafts).await?.ok_or_else(|| backend("this account has no Drafts folder"))?;
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.append(&folder, Some("(\\Draft \\Seen)"), None, &raw).await?;
            session.select(&folder).await?;
            let found = session.uid_search(format!("HEADER X-BeaverMail-Draft {id}")).await?;
            // The newest copy is the one just stored; any older copies of the same draft are dropped.
            let newest = found.iter().max().copied();
            let older: Vec<u32> = found.into_iter().filter(|u| Some(*u) != newest).collect();
            if !older.is_empty() {
                let set = older.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                // Without UIDPLUS the copies stay flagged as deleted, which clients hide.
                if let Ok(expunged) = session.uid_expunge(&set).await {
                    let _ = expunged.try_collect::<Vec<_>>().await;
                }
            }
            Ok::<Vec<u32>, async_imap::error::Error>(older)
        }
        .await;
        let older = result.map_err(|e| state.failed(e))?;
        drop(state);
        self.drop_files(&folder, &older);
        self.store.remove_uids(&self.account, &folder, &older)
    }

    async fn delete_draft(&self, id: &str) -> Result<(), MailError> {
        let Some(folder) = self.folder_with_role(Role::Drafts).await? else { return Ok(()) };
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.select(&folder).await?;
            let found: Vec<u32> = session.uid_search(format!("HEADER X-BeaverMail-Draft {id}")).await?.into_iter().collect();
            if !found.is_empty() {
                let set = found.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                if let Ok(expunged) = session.uid_expunge(&set).await {
                    let _ = expunged.try_collect::<Vec<_>>().await;
                }
            }
            Ok::<Vec<u32>, async_imap::error::Error>(found)
        }
        .await;
        let found = result.map_err(|e| state.failed(e))?;
        drop(state);
        self.drop_files(&folder, &found);
        self.store.remove_uids(&self.account, &folder, &found)
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

    async fn move_to(&self, id: MessageId, role: Role) -> Result<(), MailError> {
        self.write(id, Op::Move(role)).await
    }

    async fn erase(&self, id: MessageId) -> Result<(), MailError> {
        self.write(id, Op::Erase).await
    }

    async fn empty(&self, role: Role) -> Result<usize, MailError> {
        if !matches!(role, Role::Trash | Role::Junk) {
            return Err(backend("only Trash and Spam can be emptied"));
        }
        let folder = self.folder_with_role(role).await?.ok_or_else(|| backend(format!("this account has no {role:?} folder")))?;
        // Only what the user has seen listed: what arrived meanwhile is not erased unseen.
        let uids: Vec<u32> = self.store.flags_since(&self.account, &folder, 0)?.into_iter().map(|(uid, _)| uid).collect();
        if uids.is_empty() {
            return Ok(0);
        }
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.select(&folder).await?;
            for chunk in uids.chunks(500) {
                let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                session.uid_expunge(&set).await?.try_collect::<Vec<_>>().await?;
            }
            Ok::<(), async_imap::error::Error>(())
        }
        .await;
        result.map_err(|e| state.failed(e))?;
        self.drop_files(&folder, &uids);
        self.store.remove_uids(&self.account, &folder, &uids)?;
        Ok(uids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_FILES: &str = "From: a@x.org\r\nSubject: files\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n--r\r\nContent-Type: text/html\r\n\r\n<p>hi <img src=\"cid:logo\"></p>\r\n--r\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\nContent-Disposition: inline\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0K\r\n--r--\r\n--b\r\nContent-Type: application/pdf; name=\"plan.pdf\"\r\nContent-Disposition: attachment; filename=\"plan.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gd29ybGQ=\r\n--b\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"notes.txt\"\r\n\r\nremember\r\n--b--\r\n";

    #[test]
    fn attachments_are_listed_without_the_pictures_embedded_in_the_text() {
        let body = parse_body(WITH_FILES.as_bytes()).unwrap();
        let names: Vec<_> = body.attachments.iter().map(|a| (a.name.as_str(), a.mime.as_str(), a.size)).collect();
        assert_eq!(names, [("plan.pdf", "application/pdf", 11), ("notes.txt", "text/plain", 8)]);
    }

    #[test]
    fn an_attachment_comes_back_decoded_by_its_position() {
        let first = body_index(WITH_FILES, "plan.pdf");
        let (name, bytes) = attachment_data(WITH_FILES.as_bytes(), first).unwrap();
        assert_eq!((name.as_str(), bytes.as_slice()), ("plan.pdf", &b"Hello world"[..]));
        assert!(matches!(attachment_data(WITH_FILES.as_bytes(), 99), Err(MailError::NotFound)));
    }

    fn body_index(raw: &str, name: &str) -> u32 {
        parse_body(raw.as_bytes()).unwrap().attachments.into_iter().find(|a| a.name == name).unwrap().index
    }

    #[test]
    fn the_servers_authentication_results_and_spam_verdict_are_read_in_order() {
        let raw = "Authentication-Results: mail.example.org;\r\n\tdkim=pass header.d=Bank.com header.s=k1;\r\n\tspf=fail (reason) smtp.mailfrom=bounce@attacker.io;\r\n\tdmarc=pass (p=reject) header.from=bank.com\r\nAuthentication-Results: mx.attacker.io; dmarc=pass header.from=bank.com\r\nX-Spam-Status: Yes, score=9.1\r\nFrom: a@bank.com\r\nSubject: Authentication-Results: not a header\r\n\r\nAuthentication-Results: body text\r\n";
        let body = parse_body(raw.as_bytes()).unwrap();
        assert!(body.spam);
        assert_eq!(body.auth.len(), 2, "only header lines count, not the body");
        assert_eq!(body.auth[0].server, "mail.example.org");
        let checks: Vec<_> = body.auth[0].checks.iter().map(|c| (c.method.as_str(), c.result.as_str(), c.domain.as_str())).collect();
        assert_eq!(checks, [("dkim", "pass", "bank.com"), ("spf", "fail", "attacker.io"), ("dmarc", "pass", "bank.com")]);
        assert_eq!(body.auth[1].server, "mx.attacker.io");
        assert!(!parse_body(b"From: a@b.org\r\nX-Spam-Status: No, score=0\r\n\r\nhi").unwrap().spam);
    }

    #[test]
    fn list_unsubscribe_links_and_one_click_are_read() {
        let raw = "From: news@shop.example\r\nList-Unsubscribe: <mailto:leave@shop.example?subject=unsubscribe>,\r\n <https://shop.example/u?id=1>, <http://shop.example/plain>, <ftp://x>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\r\nhi";
        let body = parse_body(raw.as_bytes()).unwrap();
        assert_eq!(body.unsubscribe, ["mailto:leave@shop.example?subject=unsubscribe", "https://shop.example/u?id=1"], "plain http and other schemes are left out");
        assert!(body.one_click);
        let body = parse_body(b"From: a@b.org\r\nList-Unsubscribe-Post: List-Unsubscribe=Maybe\r\n\r\nhi").unwrap();
        assert!(body.unsubscribe.is_empty() && !body.one_click);
    }

    #[test]
    fn exims_spam_bar_and_spamassassins_rules_are_read() {
        let raw = "From: a@b.org\r\nX-Spam-Score: -7\r\nX-Spam-Bar: /\r\nX-Spam-Report: Spam detection software\r\n Content analysis details:   (-0.8 points, 5.0 required)\r\n  pts rule name              description\r\n ---- ---------------------- ----\r\n -3.0 RCVD_IN_VALIDITY_CERTIFIED RBL: Sender in Validity\r\n  0.0 SPF_FAIL               SPF: sender does not match SPF record (fail)\r\n\r\nhi";
        let body = parse_body(raw.as_bytes()).unwrap();
        assert!(!body.spam);
        assert_eq!(body.spam_tests, ["RCVD_IN_VALIDITY_CERTIFIED", "SPF_FAIL"]);
        assert!(parse_body(b"From: a@b.org\r\nX-Spam-Bar: ++++++\r\n\r\nhi").unwrap().spam);
        let status = parse_body(b"From: a@b.org\r\nX-Spam-Status: Yes, score=9.1 required=5.0 tests=BAYES_99,SPF_FAIL autolearn=no\r\n\r\nhi").unwrap();
        assert_eq!(status.spam_tests, ["BAYES_99", "SPF_FAIL"]);
    }

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
