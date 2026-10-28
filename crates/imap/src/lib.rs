// SPDX-License-Identifier: AGPL-3.0-only

//! IMAP backend: implements [`MailSource`] over one account.

pub mod folder_name;

use std::fmt;
use std::sync::Arc;

use async_imap::imap_proto::{AttributeValue, MailboxDatum, Response, Status};
use async_imap::types::{Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use beavermail_core::{AuthCheck, AuthResult, Account, Attachment, Encryption, Hit, Keep, MailError, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Progress, Role, Server, SignedWith, SyncReport, Tls};
use beavermail_store::{Flags, Header, Location, Store};
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
const HEADER_FIELDS: &str = "BODY.PEEK[HEADER.FIELDS (FROM TO CC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE X-PRIORITY IMPORTANCE AUTHENTICATION-RESULTS X-SPAM-STATUS X-SPAM-FLAG X-SPAM-BAR X-SPAM-SCORE X-SPAM-REPORT)]";

trait Conn: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> Conn for T {}
type Stream = Box<dyn Conn>;

#[derive(Default)]
struct State {
    session: Option<Session<Stream>>,
    /// When the session was last taken; one quiet for longer is checked before it is used again.
    used: Option<std::time::Instant>,
}

/// What a write does to one message.
#[derive(Clone)]
enum Op {
    Seen(bool),
    /// The [`PINNED`] keyword.
    Pinned(bool),
    /// `\Flagged`: the star every client shows.
    Starred(bool),
    /// A label, as a keyword: the standard `$label1`… or one of the user's own.
    Label(String, bool),
    Archive,
    Trash,
    /// To the folder with this role (Spam, or back to the Inbox).
    Move(Role),
    /// To the account's folder of this name.
    Folder(String),
    /// Permanent delete; allowed only inside the trash folder.
    Erase,
}

pub struct ImapSource {
    config: Config,
    /// How often a server without IDLE is asked whether anything changed.
    poll_interval: std::time::Duration,
    /// Identity of the account in the archive.
    account: String,
    /// The address mail comes to, whose own key opens what is encrypted to it.
    address: String,
    store: Arc<Store>,
    /// Where the downloaded messages live, encrypted.
    files: Arc<Vault>,
    state: Mutex<State>,
    /// What [`Self::auth_server`] found, and whether it was learned (then it is final) and when not.
    auth_server: std::sync::Mutex<Option<(String, Option<std::time::Instant>)>>,
    /// Messages as (folder, uid) whose flags were changed here and are on their way to the server: a
    /// sync meanwhile leaves them as they are.
    writing: std::sync::Mutex<std::collections::HashSet<(String, u32)>>,
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

    /// The message as the server holds it, every header and part: from this computer when stored, else
    /// downloaded and kept.
    pub async fn source(&self, id: MessageId) -> Result<Vec<u8>, MailError> {
        let at = self.store.locate(id)?.ok_or(MailError::NotFound)?;
        let path = self.file_of(&at.account, &at.folder, at.uid)?;
        if let Some(raw) = self.files.read(&path).ok().flatten() {
            return Ok(raw);
        }
        let raw = self.download(&at).await?;
        // As for the body: failing to keep the file does not stop the message from opening.
        if self.files.write(&path, &raw).is_ok() {
            self.store.mark_stored(&at.account, &at.folder, at.uid)?;
        }
        Ok(raw)
    }

    /// The name the account's own server writes its `Authentication-Results` under (the authserv-id),
    /// so its checks can be told from those a sender wrote: learned from the account's mail (see
    /// [`own_auth_server`]), else the IMAP server's name. Asked again every few minutes until learned.
    pub fn auth_server(&self) -> String {
        let mut known = self.auth_server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((name, asked)) = known.as_ref()
            && asked.is_none_or(|at| at.elapsed() < std::time::Duration::from_secs(600))
        {
            return name.clone();
        }
        let rows = self.store.recent_headers(&self.account, 300).unwrap_or_default();
        let found = match own_auth_server(&rows) {
            Some(name) => (name, None),
            None => (self.config.server.host.clone(), Some(std::time::Instant::now())),
        };
        *known = Some(found.clone());
        found.0
    }

    /// What the kept header fields say (sender, Reply-To, the server's checks), without text or
    /// attachments; `None` when they were not read yet.
    pub fn header_body(&self, id: MessageId) -> Result<Option<MessageBody>, MailError> {
        self.store.headers(id)?.filter(|raw| !raw.is_empty()).map(|raw| parse_body(&raw)).transpose()
    }

    /// Creates the source over the shared archive; the connection is opened on first use.
    pub fn new(account: &Account, password: String, store: Arc<Store>, files: Arc<Vault>) -> Self {
        let config = Config { server: account.imap.clone(), user: account.user.clone(), password, offline: Default::default(), token: None };
        Self { config, poll_interval: std::time::Duration::from_secs(180), account: account.key(), address: account.address().to_lowercase(), store, files, state: Mutex::new(State::default()), auth_server: std::sync::Mutex::new(None), writing: Default::default() }
    }

    /// Changes how often a server without IDLE is polled (3 minutes by default).
    #[must_use]
    /// Signs in with `token`'s access tokens (XOAUTH2) instead of the password.
    pub fn with_token(mut self, token: beavermail_core::TokenSource) -> Self {
        self.config.token = Some(token);
        self
    }

    /// The body of a message as the reader shows it: decrypted with the account's own key when it is
    /// encrypted, with what its signature says, checked against the key its sender was first seen with.
    /// A key the message announces (Autocrypt) is kept when none is yet.
    fn read(&self, raw: &[u8]) -> Result<MessageBody, MailError> {
        let known: Vec<Vec<u8>> = beavermail_crypto::sender_of(raw).and_then(|from| self.store.peer_key(&from).ok().flatten()).into_iter().collect();
        if known.is_empty()
            && let Some((address, key)) = beavermail_crypto::autocrypt_key(raw)
        {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            let _ = self.store.learn_peer_key(&address, &key, now); // not keeping it only means checking with it again next time
        }
        let (opened, kind) = match beavermail_crypto::decrypt(raw, &self.own_keys(), &known) {
            Some(opened) => (Some(opened), SignedWith::OpenPgp),
            None => (beavermail_crypto::decrypt_smime(raw, &self.own_certificates()), SignedWith::Smime),
        };
        let mut body = match opened {
            None => self.signed(raw, raw, None, &known)?,
            Some(beavermail_crypto::Opened::Message(plain, inside)) => MessageBody { encryption: Some(Encryption::Opened(kind)), ..self.signed(raw, &plain, inside, &known)? },
            Some(beavermail_crypto::Opened::NoKey) => MessageBody { encryption: Some(Encryption::NoKey(kind)), ..parse_body(raw)? },
            Some(beavermail_crypto::Opened::Failed(why)) => MessageBody { encryption: Some(Encryption::Failed(kind, why)), ..parse_body(raw)? },
        };
        if matches!(body.encryption, Some(Encryption::NoKey(_) | Encryption::Failed(..))) {
            body.attachments.clear(); // only the sealed data, of no use to open
        }
        Ok(body)
    }

    /// The body of `message` (the decrypted form of `raw`, or `raw` itself) with what its signature says:
    /// `inside`, a signature the decryption already checked, or one in the message, which may wrap its
    /// content (S/MIME in one piece). The certificate of a valid S/MIME signature is kept, to encrypt to
    /// its owner later.
    fn signed(&self, raw: &[u8], message: &[u8], inside: Option<beavermail_core::Signature>, known: &[Vec<u8>]) -> Result<MessageBody, MailError> {
        let roots = beavermail_crypto::system_roots();
        let (shown, signature) = match inside {
            Some(signature) => (message.to_vec(), Some(signature)),
            None => match beavermail_crypto::unwrap_signed(message, roots) {
                Some((content, signature)) => (content, Some(signature)),
                None => (message.to_vec(), beavermail_crypto::verify(message, known, roots)),
            },
        };
        if let Some(beavermail_core::Signature { kind: SignedWith::Smime, check: beavermail_core::SignatureCheck::Valid { signer, .. } }) = &signature
            && let Some(cert) = beavermail_crypto::signer_certificate(message).or_else(|| beavermail_crypto::signer_certificate(raw))
        {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            let _ = self.store.learn_peer_cert(signer, &cert, now);
        }
        Ok(MessageBody { signature, ..parse_body(&shown)? })
    }

    /// The account's own S/MIME certificate, from the vault.
    fn own_certificates(&self) -> Vec<Vec<u8>> {
        self.files.read(&beavermail_crypto::certificate_path(&self.address)).ok().flatten().into_iter().collect()
    }

    /// The account's own OpenPGP key, from the vault; none when encryption is not set up (or the vault is locked).
    fn own_keys(&self) -> Vec<Vec<u8>> {
        self.files.read(&beavermail_crypto::key_path(&self.address)).ok().flatten().into_iter().collect()
    }

    /// Signs in and out on a connection of its own, touching nothing: whether the server, its TLS and
    /// the login work.
    pub async fn check(&self) -> Result<(), MailError> {
        let mut session = connect_in_time(&self.config).await?;
        let _ = session.logout().await;
        Ok(())
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
    /// The server's separator between a folder and those inside it, from an empty LIST (RFC 9051 6.3.9).
    async fn delimiter(&self, state: &mut State) -> Result<Option<char>, MailError> {
        let listed = answered(state.session(&self.config).await?, "LIST \"\" \"\"", |r| match r {
            Response::MailboxData(MailboxDatum::List(list)) => list.delimiter.as_deref().and_then(|d| d.chars().next()),
            _ => None,
        })
        .await;
        match listed {
            Ok(found) => Ok(found.into_iter().next()),
            Err(e) => Err(state.failed(e)),
        }
    }

    /// Renames folder `from` to `to` on the server, then here: the folders inside it and their messages
    /// follow, keeping their ids, flags and files. `to` is checked already.
    async fn move_folder(&self, mut state: tokio::sync::MutexGuard<'_, State>, from: &str, to: &str, delimiter: Option<char>) -> Result<String, MailError> {
        let renamed = state.session(&self.config).await?.rename(from, to).await;
        if let Err(e) = renamed {
            return Err(state.failed(e));
        }
        let _ = state.session(&self.config).await?.subscribe(to).await;
        drop(state);
        // Without a separator the server keeps no folders inside others, so any will do.
        for (old, new) in self.store.rename_folder(&self.account, from, to, delimiter.unwrap_or('/'))? {
            self.move_files(&old, &new);
        }
        Ok(to.to_owned())
    }

    /// Moves a renamed folder's stored messages to its new name. They are encrypted for their place, so
    /// each is written again there; one that cannot be (the vault locked) is downloaded again when opened.
    fn move_files(&self, old: &str, new: &str) {
        let (from, to) = (folder_path(&self.account, old), folder_path(&self.account, new));
        for path in self.files.list(&from).unwrap_or_default() {
            let (Some(file), Ok(Some(data))) = (path.file_name(), self.files.read(&path)) else { continue };
            if self.files.write(&to.join(file), &data).is_ok() {
                let _ = self.files.remove(&path);
            }
        }
        let _ = self.files.remove_dir(&from);
    }

    fn drop_files(&self, folder: &str, uids: &[u32]) {
        for uid in uids {
            if let Ok(path) = self.file_of(&self.account, folder, *uid) {
                let _ = self.files.remove(&path); // a leftover file is harmless; the next reset removes the folder
            }
        }
    }
}

/// Why the Inbox and the folders with a role keep their names: the app finds some of them by name.
const KEEPS_NAME: &str = "the Inbox and the folders for sent mail, drafts, the archive, spam and the trash keep their names";

/// A folder name the user typed, against what RFC 9051 (5.1) forbids or advises against, and the
/// server's separator, which cannot be inside one level's name.
fn check_name(name: &str, delimiter: Option<char>) -> Result<(), MailError> {
    let refused = match name {
        "" => "the folder needs a name".to_owned(),
        _ if name.chars().any(char::is_control) => "a folder name cannot hold control characters".to_owned(),
        _ if name.contains(['%', '*']) => "a folder name cannot hold % or *: servers read them as wildcards".to_owned(),
        _ if name.starts_with('#') => "a folder name cannot start with #: servers use it for shared folders".to_owned(),
        _ => match delimiter.filter(|d| name.contains(*d)) {
            Some(d) => format!("a folder name cannot contain \"{d}\" on this server: it separates a folder from those inside it"),
            None => return Ok(()),
        },
    };
    Err(backend(refused))
}

fn backend(e: impl fmt::Display) -> MailError {
    MailError::Backend(e.to_string())
}

/// An IMAP error as the app tells it: a connection that ended (the server restarting, the network
/// gone) is being offline for a while, to be tried again, not a failure.
fn imap_error(e: async_imap::error::Error) -> MailError {
    match e {
        async_imap::error::Error::ConnectionLost | async_imap::error::Error::Io(_) => MailError::Offline(e.to_string()),
        e => backend(e),
    }
}

/// Runs `command` and keeps what `take` finds in its answer, only once the command's tagged OK came.
/// The library's streams just end when the connection closes (after a BYE, say): an answer cut short
/// would read as a complete one, and what it misses would be forgotten here as gone from the server.
async fn answered<T>(session: &mut Session<Stream>, command: &str, mut take: impl FnMut(&Response<'_>) -> Option<T>) -> Result<Vec<T>, async_imap::error::Error> {
    let tag = session.run_command(command).await?;
    let mut found = Vec::new();
    loop {
        let Some(response) = session.read_response().await? else { return Err(async_imap::error::Error::ConnectionLost) };
        match response.parsed() {
            Response::Done { tag: done, status, outcome } if *done == tag => {
                let why = || outcome.information.as_deref().unwrap_or_default().to_owned();
                return match status {
                    Status::Ok => Ok(found),
                    Status::No => Err(async_imap::error::Error::No(why())),
                    _ => Err(async_imap::error::Error::Bad(why())),
                };
            }
            parsed => found.extend(take(parsed)),
        }
    }
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

/// How long a server may take to accept the connection.
const CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long a session may stay unused before it is checked with a NOOP.
const QUIET_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// A connection that could not be made, said so the user knows what to check. Still offline: a
/// server that is down refuses too, and changes made meanwhile wait for it.
fn unreachable(host: &str, port: u16, e: &std::io::Error) -> MailError {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => MailError::Offline(format!("{host} refused the connection on port {port}: check the port, or the server is down")),
        // The lookup's own error has no kind of its own: its text says so on Linux and macOS, its code on
        // Windows (WSAHOST_NOT_FOUND, WSATRY_AGAIN, WSANO_DATA).
        _ if e.to_string().contains("lookup") || matches!(e.raw_os_error(), Some(11001 | 11002 | 11004)) =>MailError::Offline(format!("{host} could not be found: check the server name, or the connection")),
        _ => MailError::Offline(format!("{host} could not be reached: {e}")),
    }
}

/// [`connect`], given up after a while: a server that accepts the connection and then stalls (in TLS,
/// its greeting or the login) would otherwise hold the account's session, and every call waiting on
/// it, for ever.
async fn connect_in_time(c: &Config) -> Result<Session<Stream>, MailError> {
    tokio::time::timeout(2 * CONNECT_WAIT, connect(c)).await.map_err(|_| MailError::Offline(format!("{} stopped answering", c.server.host)))?
}

async fn connect(c: &Config) -> Result<Session<Stream>, MailError> {
    if c.offline.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(MailError::Offline("working offline".into()));
    }
    let Server { host, port, tls: mode } = &c.server;
    let tcp = match tokio::time::timeout(CONNECT_WAIT, TcpStream::connect((host.as_str(), *port))).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => return Err(unreachable(host, *port, &e)),
        Err(_) => return Err(MailError::Offline(format!("{host} did not answer on port {port}"))),
    };
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
        // Servers end quiet sessions (a BYE for inactivity): one that does not answer a NOOP is replaced
        // here, instead of failing the call and showing the account offline.
        if let Some(session) = self.session.as_mut().filter(|_| self.used.is_some_and(|t| t.elapsed() > QUIET_CHECK))
            && !matches!(tokio::time::timeout(CONNECT_WAIT, session.noop()).await, Ok(Ok(())))
        {
            self.session = None;
        }
        if self.session.is_none() {
            self.session = Some(connect_in_time(config).await?);
        }
        self.used = Some(std::time::Instant::now());
        Ok(self.session.as_mut().expect("just connected"))
    }

    /// Maps an IMAP error and drops the session, so the next call reconnects.
    fn failed(&mut self, e: async_imap::error::Error) -> MailError {
        self.session = None;
        imap_error(e)
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

/// A header block as the archive stores it, or `None` when the fetch has no UID or header, or the
/// message is flagged deleted (another client deleted it and has not expunged it yet: it is gone).
fn header(fetch: &Fetch) -> Option<Header> {
    if fetch.flags().any(|f| f == Flag::Deleted) {
        return None;
    }
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
        pinned: fetch.flags().any(|f| matches!(f, Flag::Custom(k) if k == PINNED)),
        starred: fetch.flags().any(|f| f == Flag::Flagged),
        priority: priority(&message),
        labels: labels(fetch.flags()),
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

/// What the sender marked a message as: 1 high, -1 low, 0 neither. `X-Priority` gives 1 (highest) to 5
/// (lowest), `Importance` high or low.
fn priority(message: &mail_parser::Message<'_>) -> i8 {
    let level = message.header_raw("X-Priority").and_then(|v| v.trim().chars().next()?.to_digit(10));
    match (level, message.header_raw("Importance").map(|v| v.trim().to_ascii_lowercase())) {
        (Some(1 | 2), _) => 1,
        (Some(4 | 5), _) => -1,
        (_, Some(i)) if i == "high" => 1,
        (_, Some(i)) if i == "low" => -1,
        _ => 0,
    }
}

/// The labels among a message's keywords, separated by spaces: the standard ones (`$label1` to
/// `$label5`) and the user's own, which carry no `$`. Keywords that only record what a client did
/// (forwarded, a receipt sent, its own spam verdict) and the pin are left out.
fn labels<'a>(flags: impl Iterator<Item = Flag<'a>>) -> String {
    const NOT_LABELS: [&str; 5] = ["junk", "nonjunk", "notjunk", "forwarded", "phishing"];
    flags
        .filter_map(|f| match f {
            Flag::Custom(k) if k.len() == 7 && k.starts_with("$label") && k.ends_with(['1', '2', '3', '4', '5']) => Some(k.into_owned()),
            Flag::Custom(k) if !k.starts_with('$') && !k.starts_with('\\') && !NOT_LABELS.contains(&k.to_ascii_lowercase().as_str()) => Some(k.into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
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
            (name, Some(address)) => Some(beavermail_core::address_text(name.unwrap_or_default(), address)),
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
        let target = match &op {
            Op::Seen(_) | Op::Pinned(_) | Op::Starred(_) | Op::Label(..) | Op::Erase => None,
            Op::Archive => Some(self.folder_with_role(Role::Archive).await?.ok_or_else(|| backend("this account has no archive folder"))?),
            Op::Trash => Some(self.folder_with_role(Role::Trash).await?.ok_or_else(|| backend("this account has no trash folder"))?),
            Op::Move(role) => Some(self.folder_with_role(*role).await?.ok_or_else(|| backend(format!("this account has no {role:?} folder")))?),
            // One removed or renamed meanwhile (on the server, or from another client) is not created again.
            Op::Folder(name) => {
                if self.store.folders(&self.account)?.is_empty() {
                    self.mailboxes().await?;
                }
                Some(self.store.folders(&self.account)?.into_iter().find(|f| f.name == *name).ok_or_else(|| backend(format!("there is no folder {name} any more")))?.name)
            }
        };
        if matches!(op, Op::Erase) && self.folder_with_role(Role::Trash).await?.as_deref() != Some(at.folder.as_str()) {
            return Err(backend("only messages in Trash can be erased"));
        }
        if target.as_deref() == Some(at.folder.as_str()) {
            return Err(backend(format!("the message is already in {}", at.folder)));
        }
        // A flag is set here at once, so a list read meanwhile shows it; undone if the server refuses,
        // kept when it cannot be reached (the change waits to be sent).
        let flag = target.is_none() && !matches!(op, Op::Erase);
        let key = (at.folder.clone(), at.uid);
        if flag {
            self.set_flag(id, &op, true)?;
            self.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.clone());
        }
        let result = self.send(&at, &op, target).await;
        if flag {
            self.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&key);
            if result.as_ref().is_err_and(|e| !matches!(e, MailError::Offline(_))) {
                let _ = self.set_flag(id, &op, false);
            }
            return result;
        }
        result?;
        self.drop_files(&at.folder, &[at.uid]);
        self.store.remove(id)
    }

    /// Mirrors a flag change in the archive (`forward`), or undoes it.
    fn set_flag(&self, id: MessageId, op: &Op, forward: bool) -> Result<(), MailError> {
        match op {
            Op::Seen(seen) => self.store.set_unread(id, *seen != forward),
            Op::Pinned(on) => self.store.set_pinned(id, *on == forward),
            Op::Starred(on) => self.store.set_starred(id, *on == forward),
            Op::Label(keyword, on) => self.store.set_label(id, keyword, *on == forward),
            Op::Archive | Op::Trash | Op::Move(_) | Op::Folder(_) | Op::Erase => Ok(()),
        }
    }

    /// Makes `op` on the server.
    async fn send(&self, at: &Location, op: &Op, target: Option<String>) -> Result<(), MailError> {
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            let selected = session.select(&at.folder).await?;
            let uid = at.uid.to_string();
            match (op, target) {
                (Op::Seen(seen), _) => {
                    let query = if *seen { "+FLAGS.SILENT (\\Seen)" } else { "-FLAGS.SILENT (\\Seen)" };
                    session.uid_store(&uid, query).await?.try_collect::<Vec<_>>().await?;
                }
                (Op::Starred(on), _) => {
                    let query = if *on { "+FLAGS.SILENT (\\Flagged)" } else { "-FLAGS.SILENT (\\Flagged)" };
                    session.uid_store(&uid, query).await?.try_collect::<Vec<_>>().await?;
                }
                // A server that takes no keywords of its own keeps no pin: it stays on this computer.
                (Op::Pinned(on), _) if takes_keywords(&selected.permanent_flags) => {
                    let query = format!("{}FLAGS.SILENT ({PINNED})", if *on { "+" } else { "-" });
                    session.uid_store(&uid, query).await?.try_collect::<Vec<_>>().await?;
                }
                // A label too stays on this computer where the server takes no keywords of its own.
                (Op::Label(keyword, on), _) if takes_keywords(&selected.permanent_flags) => {
                    let query = format!("{}FLAGS.SILENT ({keyword})", if *on { "+" } else { "-" });
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
            let all: Vec<u32> = store.flags_since(account, folder, 0)?.into_iter().map(|(u, ..)| u).collect();
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
            // What is missing from the answer is forgotten here, so only a complete one counts.
            let fetches: Vec<(u32, Vec<Flag>)> = answered(session, &format!("UID FETCH {from}:* (UID FLAGS)"), |r| match r {
                Response::Fetch(_, attributes) => {
                    let uid = attributes.iter().find_map(|a| match a { AttributeValue::Uid(uid) => Some(*uid), _ => None })?;
                    let flags = attributes.iter().find_map(|a| match a { AttributeValue::Flags(f) => Some(f.iter().map(|f| Flag::from(f.to_string())).collect()), _ => None });
                    Some((uid, flags.unwrap_or_default()))
                }
                _ => None,
            })
            .await?;
            // One flagged deleted is gone as if expunged.
            // Where the server keeps no keywords, the pins and labels made here stay as they are.
            let keywords = takes_keywords(&selected.permanent_flags);
            let local = store.flags_since(account, folder, from)?;
            let kept: std::collections::HashMap<u32, &Flags> = local.iter().map(|(uid, f)| (*uid, f)).collect();
            let server: std::collections::HashMap<u32, Flags> = fetches
                .iter()
                .filter(|(_, flags)| !flags.contains(&Flag::Deleted))
                .map(|(uid, flags)| {
                    let has = |flag: Flag| flags.contains(&flag);
                    let (pinned, labels) = match kept.get(uid) {
                        Some(here) if !keywords => (here.pinned, here.labels.clone()),
                        _ => (has(Flag::Custom(PINNED.into())), labels(flags.iter().cloned())),
                    };
                    (*uid, Flags { unread: !has(Flag::Seen), pinned, starred: has(Flag::Flagged), labels })
                })
                .collect();
            let gone: Vec<u32> = local.iter().filter(|(uid, _)| !server.contains_key(uid)).map(|(uid, _)| *uid).collect();
            // One being written now keeps what it was set to here.
            let writing = self.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            let changed: Vec<(u32, Flags)> = local
                .iter()
                .filter(|(uid, _)| !writing.contains(&(folder.to_owned(), *uid)))
                .filter_map(|(uid, flags)| server.get(uid).filter(|s| *s != flags).map(|s| (*uid, s.clone())))
                .collect();
            self.drop_files(folder, &gone);
            store.remove_uids(account, folder, &gone)?;
            store.set_flags_by_uid(account, folder, &changed)?;
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
    // Sealed, its parts are only the encryption's version line and data: nothing to show as text.
    let sealed = message.root_part().content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("multipart") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("encrypted")));
    Ok(MessageBody {
        attachments: attachments(&message),
        text: if sealed { String::new() } else { message.body_text(0).map(|t| t.into_owned()).unwrap_or_default() },
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
        signature: None, // checked by the source, which knows the sender's keys
        encryption: None,
        calendar: calendar_part(&message),
    })
}

/// The first `text/calendar` part of a message, inline or attached (`.ics`), as text.
fn calendar_part(message: &mail_parser::Message<'_>) -> Option<String> {
    message
        .parts
        .iter()
        .find(|part| {
            let calendar = part.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("text") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("calendar")));
            calendar || part.attachment_name().is_some_and(|n| n.to_ascii_lowercase().ends_with(".ics"))
        })
        .map(|part| String::from_utf8_lossy(part.contents()).into_owned())
        .filter(|text| text.contains("BEGIN:VCALENDAR"))
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

/// The keyword a pin is kept under on the server, where it takes keywords of its own.
const PINNED: &str = "$Pinned";

/// The mailbox lets clients set keywords of their own (`\*` in its permanent flags) or knows the pin.
fn takes_keywords(permanent: &[Flag<'_>]) -> bool {
    permanent.iter().any(|f| matches!(f, Flag::MayCreate) || matches!(f, Flag::Custom(k) if k == PINNED))
}

/// The authserv-id of the account's own server, from its latest messages as (sender, header fields):
/// the server that receives the mail writes its `Authentication-Results` on top of any the message
/// came with, so the name on top of nearly all of them, from many senders, is its own. A sender can
/// write anything, but not on top, and not on everyone's mail. `None` while too few messages say.
fn own_auth_server(rows: &[(String, Vec<u8>)]) -> Option<String> {
    let mut seen: std::collections::HashMap<String, (usize, std::collections::HashSet<&str>)> = std::collections::HashMap::new();
    let mut total = 0;
    for (from, raw) in rows {
        let Some(top) = trace_headers(raw).into_iter().find(|(n, _)| n == "authentication-results") else { continue };
        let name = auth_result(&top.1).server;
        total += 1;
        // Microsoft's servers write no name: their first field is already a check.
        if name.is_empty() || name.contains('=') {
            continue;
        }
        let entry = seen.entry(name).or_default();
        entry.0 += 1;
        entry.1.insert(from.rsplit('@').next().unwrap_or(""));
    }
    let (name, (count, senders)) = seen.into_iter().max_by_key(|(_, (count, _))| *count)?;
    (total >= 5 && count * 10 >= total * 9 && senders.len() >= 5).then_some(name)
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
        Ok(self.store.folders(&self.account)?.into_iter().map(|f| Mailbox { name: f.name, unread: f.unread, role: f.role, total: f.total, stored: f.stored, delimiter: f.delimiter }).collect())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, MailError> {
        self.store.search(&self.account, query, limit)
    }

    async fn mailboxes(&self) -> Result<Vec<Mailbox>, MailError> {
        let mut state = self.state.lock().await;
        let result = async {
            let session = state.session(&self.config).await?;
            let listed: Vec<(String, Role, Option<char>)> = answered(session, "LIST \"\" *", |r| match r {
                Response::MailboxData(MailboxDatum::List(list)) if !list.name_attributes.contains(&NameAttribute::NoSelect) => {
                    Some((list.name.to_string(), role_of(&list.name_attributes), list.delimiter.as_deref().and_then(|d| d.chars().next())))
                }
                _ => None,
            })
            .await
            .map_err(imap_error)?;
            // The separator comes with each folder (RFC 9051 6.3.9); a "." is part of a name where it is "/".
            let delimiters: std::collections::HashMap<String, Option<char>> = listed.iter().map(|(name, _, d)| (name.clone(), *d)).collect();
            let mut folders: Vec<(String, Role)> = listed.into_iter().map(|(name, role, _)| (name, role)).collect();
            // Every account has an Inbox: a list without one (a server still starting) is not taken
            // as every folder gone, which would forget them and their messages here.
            if !folders.iter().any(|(n, _)| n.eq_ignore_ascii_case("INBOX")) {
                return Err(backend("the server listed no Inbox"));
            }
            folders.sort_by_key(|(n, _)| (n != "INBOX", n.clone()));
            resolve_roles(&mut folders);
            let mut mailboxes = Vec::with_capacity(folders.len());
            for (name, role) in &folders {
                let status = session.status(name, "(UNSEEN MESSAGES)").await.map_err(imap_error)?;
                let delimiter = delimiters.get(name).copied().flatten();
                mailboxes.push(Mailbox { name: name.clone(), unread: status.unseen.unwrap_or(0), role: *role, total: status.exists, stored: 0, delimiter });
            }
            Ok((folders, mailboxes))
        }
        .await;
        match result {
            Ok((folders, mut mailboxes)) => {
                self.store.set_folders(&self.account, &folders)?;
                for m in &mut mailboxes {
                    self.store.set_total(&self.account, &m.name, m.total)?;
                    self.store.set_delimiter(&self.account, &m.name, m.delimiter)?;
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
        let mut session = connect_in_time(&self.config).await?;
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
        match self.files.read(&path) {
            Ok(Some(raw)) => return self.read(&raw),
            Ok(None) => {}
            Err(e) => log::warn!("message {}: its stored file cannot be read ({e}), so it is downloaded again", id.0),
        }
        let started = std::time::Instant::now();
        let raw = self.download(&at).await?;
        log::debug!("message {}: downloaded in {:.0?}, {} bytes", id.0, started.elapsed(), raw.len());
        let body = self.read(&raw)?;
        // Not being able to keep the file (a locked vault) must not stop the message from opening.
        let kept = self.files.write(&path, &raw);
        if let Err(e) = &kept {
            log::warn!("message {}: cannot be kept on this computer ({e}), so it is downloaded at every opening", id.0);
        }
        if kept.is_ok() {
            self.store.mark_stored(&at.account, &at.folder, at.uid)?;
            // What was decrypted is never kept: an encrypted message is indexed by its subject and sender only.
            if body.encryption.is_none() {
                self.store.set_preview(id, &body.text)?;
                self.store.set_attachments(&at.account, &at.folder, at.uid, !body.attachments.is_empty())?;
                self.store.index_body(id, &body.text)?;
            }
        }
        Ok(body)
    }

    async fn attachment(&self, id: MessageId, index: u32) -> Result<(String, Vec<u8>), MailError> {
        let raw = self.source(id).await?;
        // An attachment of an encrypted message is in the decrypted content, which is where its index points.
        let plain = match beavermail_crypto::decrypt(&raw, &self.own_keys(), &[]).or_else(|| beavermail_crypto::decrypt_smime(&raw, &self.own_certificates())) {
            Some(beavermail_crypto::Opened::Message(plain, _)) => plain,
            _ => raw,
        };
        // Signed in one piece, the attachments are inside the signature.
        match beavermail_crypto::unwrap_signed(&plain, &[]) {
            Some((content, _)) => attachment_data(&content, index),
            None => attachment_data(&plain, index),
        }
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

    async fn set_label(&self, id: MessageId, keyword: &str, on: bool) -> Result<(), MailError> {
        // A keyword is an IMAP atom: nothing that would break the command.
        if keyword.is_empty() || keyword.chars().any(|c| c.is_whitespace() || c.is_control() || "(){%*\"\\]".contains(c)) {
            return Err(backend(format!("not a label name: {keyword}")));
        }
        self.write(id, Op::Label(keyword.to_owned(), on)).await
    }

    async fn set_starred(&self, id: MessageId, starred: bool) -> Result<(), MailError> {
        self.write(id, Op::Starred(starred)).await
    }

    async fn set_pinned(&self, id: MessageId, pinned: bool) -> Result<(), MailError> {
        self.write(id, Op::Pinned(pinned)).await
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

    async fn move_to_folder(&self, id: MessageId, folder: &str) -> Result<(), MailError> {
        self.write(id, Op::Folder(folder.to_owned())).await
    }

    async fn create_folder(&self, parent: Option<&str>, name: &str) -> Result<String, MailError> {
        let name = name.trim();
        let mut state = self.state.lock().await;
        let delimiter = self.delimiter(&mut state).await?;
        check_name(name, delimiter)?;
        let full = match (parent, delimiter) {
            (None, _) => folder_name::encode(name),
            (Some(parent), Some(d)) => format!("{parent}{d}{}", folder_name::encode(name)),
            (Some(_), None) => return Err(backend("this server keeps no folders inside others")),
        };
        if self.store.folders(&self.account)?.iter().any(|f| f.name.eq_ignore_ascii_case(&full)) {
            return Err(backend(format!("there is a folder {name} already")));
        }
        let created = state.session(&self.config).await?.create(&full).await;
        if let Err(e) = created {
            return Err(state.failed(e));
        }
        // Listed by clients that show subscribed folders only; a server without subscriptions is fine.
        let _ = state.session(&self.config).await?.subscribe(&full).await;
        Ok(full)
    }

    async fn rename_folder(&self, from: &str, name: &str) -> Result<String, MailError> {
        let name = name.trim();
        let folders = self.store.folders(&self.account)?;
        let folder = folders.iter().find(|f| f.name == from).ok_or_else(|| backend(format!("there is no folder {from} any more")))?;
        if folder.role != Role::Other {
            return Err(backend(KEEPS_NAME));
        }
        let mut state = self.state.lock().await;
        let delimiter = match folder.delimiter {
            Some(d) => Some(d),
            None => self.delimiter(&mut state).await?,
        };
        check_name(name, delimiter)?;
        // Where it is: only its own name changes.
        let to = match delimiter.and_then(|d| from.rfind(d).map(|at| (d, at))) {
            Some((d, at)) => format!("{}{d}{}", &from[..at], folder_name::encode(name)),
            None => folder_name::encode(name),
        };
        if to == from {
            return Ok(to);
        }
        if folders.iter().any(|f| f.name != from && f.name.eq_ignore_ascii_case(&to)) {
            return Err(backend(format!("there is a folder {name} already")));
        }
        self.move_folder(state, from, &to, delimiter).await
    }

    async fn trash_folder(&self, name: &str) -> Result<String, MailError> {
        let trash = self.folder_with_role(Role::Trash).await?.ok_or_else(|| backend("this account has no trash folder"))?;
        let folders = self.store.folders(&self.account)?;
        let folder = folders.iter().find(|f| f.name == name).ok_or_else(|| backend(format!("there is no folder {name} any more")))?;
        if folder.role != Role::Other {
            return Err(backend("the Inbox and the folders for sent mail, drafts, the archive, spam and the trash cannot be deleted"));
        }
        let mut state = self.state.lock().await;
        let delimiter = match folder.delimiter {
            Some(d) => d,
            None => self.delimiter(&mut state).await?.ok_or_else(|| backend("this server keeps no folders inside others, so none in the trash"))?,
        };
        if name.strip_prefix(&trash).is_some_and(|rest| rest.starts_with(delimiter)) {
            return Err(backend("the folder is in the trash already"));
        }
        // Its own name inside the trash, numbered when the trash holds one of that name already.
        let own = name.rfind(delimiter).map_or(name, |at| &name[at + delimiter.len_utf8()..]);
        let taken = |to: &str| folders.iter().any(|f| f.name.eq_ignore_ascii_case(to));
        let to = std::iter::once(format!("{trash}{delimiter}{own}"))
            .chain((2..).map(|n| format!("{trash}{delimiter}{own} {n}")))
            .find(|to| !taken(to))
            .expect("some number is free");
        self.move_folder(state, name, &to, Some(delimiter)).await
    }

    async fn delete_folder(&self, name: &str) -> Result<(), MailError> {
        let folders = self.store.folders(&self.account)?;
        let folder = folders.iter().find(|f| f.name == name).ok_or_else(|| backend(format!("there is no folder {name} any more")))?;
        if folder.role != Role::Other {
            return Err(backend("the Inbox and the folders for sent mail, drafts, the archive, spam and the trash cannot be deleted"));
        }
        // Those inside first, deepest first: a server may refuse a folder that still holds others.
        let mut doomed: Vec<&str> = match folder.delimiter {
            Some(d) => folders.iter().map(|f| f.name.as_str()).filter(|f| f.strip_prefix(name).is_some_and(|rest| rest.starts_with(d))).collect(),
            None => Vec::new(),
        };
        doomed.sort_by_key(|f| std::cmp::Reverse(f.len()));
        doomed.push(name);
        let mut state = self.state.lock().await;
        for folder in doomed {
            let deleted = state.session(&self.config).await?.delete(folder).await;
            if let Err(e) = deleted {
                return Err(state.failed(e));
            }
            let _ = state.session(&self.config).await?.unsubscribe(folder).await;
            self.store.forget_folder(&self.account, folder)?;
            let _ = self.files.remove_dir(&folder_path(&self.account, folder));
        }
        Ok(())
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
        let listed: Vec<u32> = self.store.flags_since(&self.account, &folder, 0)?.into_iter().map(|(uid, ..)| uid).collect();
        let mut state = self.state.lock().await;
        let session = state.session(&self.config).await?;
        let result = async {
            session.select(&folder).await?;
            // And what another client already deleted without expunging: not listed, but still counted.
            let mut uids = listed.clone();
            uids.extend(session.uid_search("DELETED").await?.into_iter().filter(|u| !listed.contains(u)));
            for chunk in uids.chunks(500) {
                let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                session.uid_expunge(&set).await?.try_collect::<Vec<_>>().await?;
            }
            Ok::<(), async_imap::error::Error>(())
        }
        .await;
        result.map_err(|e| state.failed(e))?;
        self.drop_files(&folder, &listed);
        self.store.remove_uids(&self.account, &folder, &listed)?;
        Ok(listed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invitation_part_is_read_from_the_message() {
        let raw = "From: ada@example.org\r\nSubject: Invitation\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nYou are invited\r\n--b\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let body = parse_body(raw.as_bytes()).unwrap();
        assert!(body.calendar.as_deref().is_some_and(|c| c.contains("METHOD:REQUEST")));
        assert_eq!(parse_body(b"From: a@b.c\r\nSubject: x\r\n\r\nhello").unwrap().calendar, None);
    }

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
    fn the_senders_priority_and_the_labels_of_other_clients_are_read() {
        let level = |raw: &str| priority(&MessageParser::default().parse_headers(raw.as_bytes()).unwrap());
        assert_eq!(level("X-Priority: 1 (Highest)\r\nSubject: a\r\n\r\n"), 1);
        assert_eq!(level("X-Priority: 5 (Lowest)\r\nSubject: a\r\n\r\n"), -1);
        assert_eq!(level("Importance: High\r\nSubject: a\r\n\r\n"), 1);
        assert_eq!(level("X-Priority: 3 (Normal)\r\nSubject: a\r\n\r\n"), 0);
        assert_eq!(level("Subject: a\r\n\r\n"), 0);
        let flags = [Flag::Seen, Flag::Custom("$label1".into()), Flag::Custom("clients".into()), Flag::Custom("$Forwarded".into()), Flag::Custom("NonJunk".into()), Flag::Custom(PINNED.into()), Flag::Custom("$label9".into())];
        assert_eq!(labels(flags.into_iter()), "$label1 clients");
    }

    #[test]
    fn a_pin_goes_to_the_server_only_where_it_takes_keywords_of_its_own() {
        assert!(!takes_keywords(&[Flag::Seen, Flag::Flagged, Flag::Deleted]), "only the system flags: the pin stays here");
        assert!(takes_keywords(&[Flag::Seen, Flag::MayCreate]));
        assert!(takes_keywords(&[Flag::Custom(PINNED.into())]));
    }

    #[test]
    fn the_accounts_own_server_is_the_name_on_top_of_nearly_all_its_mail() {
        let mail = |from: &str, tops: &[&str]| {
            let headers: String = tops.iter().map(|t| format!("Authentication-Results: {t}; spf=pass smtp.mailfrom={from}\r\n")).collect();
            (from.to_owned(), format!("{headers}From: {from}\r\n\r\n").into_bytes())
        };
        let senders = ["a@one.com", "b@two.org", "c@three.net", "d@four.io", "e@five.de", "f@six.it", "g@seven.fr", "h@eight.es", "i@nine.nl", "j@ten.ch"];
        // What the sender wrote sits below the receiving server's own.
        let mut rows: Vec<_> = senders.iter().map(|s| mail(s, &["mx.google.com", "mx.sender.example"])).collect();
        assert_eq!(own_auth_server(&rows).as_deref(), Some("mx.google.com"));
        rows.push(mail("k@eleven.com", &["mx.attacker.io"]));
        assert_eq!(own_auth_server(&rows).as_deref(), Some("mx.google.com"), "one message in eleven does not change it");
        // A server that writes none: each message carries its sender's, and no name wins.
        let none: Vec<_> = senders.iter().enumerate().map(|(i, s)| mail(s, &[&format!("mx{i}.example")])).collect();
        assert_eq!(own_auth_server(&none), None);
        // One sender's newsletters, however many, are not the whole mailbox.
        let one: Vec<_> = (0..10).map(|_| mail("news@shop.com", &["mta.shop.com"])).collect();
        assert_eq!(own_auth_server(&one), None);
        assert_eq!(own_auth_server(&rows[..3]), None, "too few to say");
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
