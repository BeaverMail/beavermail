// SPDX-License-Identifier: AGPL-3.0-only

//! Domain model and the boundary between the UI and mail sources.
//!
//! This crate must not depend on any GUI or network code. Protocol backends (JMAP, IMAP,
//! POP3) implement [`MailSource`] in their own crates.

pub mod model;
pub mod sample;

pub use model::{address_text, OAuthProvider, Account, AddressBook, Attachment, CalendarSource, AuthCheck, AuthResult, DisplayMode, Hit, Keep, Mailbox, MessageBody, MessageId, MessageSummary, Outgoing, OutgoingFile, Progress, Role, Encryption, Seal, Server, Settings, Signature, SignatureCheck, SignedWith, SyncReport, Tls};

use std::fmt;
use std::future::Future;

/// Why a [`MailSource`] call failed. Backends map their own errors onto these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailError {
    /// The mailbox or message does not exist.
    NotFound,
    /// Credentials were rejected or a token expired.
    Auth(String),
    /// Network, protocol or storage failure.
    Backend(String),
    /// The server could not be reached (no network, or it is down): the archive still answers.
    Offline(String),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Auth(why) => write!(f, "authentication failed: {why}"),
            Self::Backend(why) => write!(f, "mail backend error: {why}"),
            Self::Offline(why) => write!(f, "cannot reach the server: {why}"),
        }
    }
}

impl std::error::Error for MailError {}

/// A fresh OAuth 2.0 access token for an account that signs in with a provider: the backends ask it
/// before each connection. It may ask the provider for a new token, so it blocks; call it off the
/// async threads.
pub type TokenSource = std::sync::Arc<dyn Fn() -> Result<String, MailError> + Send + Sync>;

/// Access to one account's mail. The UI talks only to this trait.
///
/// Methods return `Send` futures so a backend can be driven from any executor;
/// implementors can still write plain `async fn`.
pub trait MailSource {
    /// Mailboxes (folders) as the local archive knows them, without touching the network.
    fn cached_mailboxes(&self) -> impl Future<Output = Result<Vec<Mailbox>, MailError>> + Send;

    /// Searches the local archive of the account: words (in sender, subject, preview and message text),
    /// `from:`, `subject:`, `in:<folder>`, `is:unread`, `has:attachment`. Newest first.
    fn search(&self, query: &str, limit: usize) -> impl Future<Output = Result<Vec<Hit>, MailError>> + Send;

    /// Asks the server for the mailboxes, remembers them locally and returns them.
    fn mailboxes(&self) -> impl Future<Output = Result<Vec<Mailbox>, MailError>> + Send;

    /// Summaries of the messages in `mailbox` from the local archive, newest first. Instant and
    /// available offline; [`MailSource::sync`] is what brings the archive up to date.
    fn messages(&self, mailbox: &str) -> impl Future<Output = Result<Vec<MessageSummary>, MailError>> + Send;

    /// Brings `mailbox` in the local archive up to date: new messages, changed flags, deleted
    /// messages, and one more chunk of older history. Call again while
    /// [`SyncReport::remaining`] is not zero.
    fn sync(&self, mailbox: &str) -> impl Future<Output = Result<SyncReport, MailError>> + Send;

    /// Watches `mailbox` and calls `changed` whenever the server reports something there (new mail,
    /// deletions, flag changes), using IDLE where the server has it and polling where not. Returns
    /// `Ok` once `stop` is set; an error means the connection is lost and the caller should retry.
    fn watch(&self, mailbox: &str, changed: &(dyn Fn() + Send + Sync), stop: &std::sync::atomic::AtomicBool) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Downloads the messages `keep` asks for, newest first, folder by folder, after synchronising
    /// the folders far enough back. Reports through `progress`, waits while it is paused and returns
    /// early when it is cancelled.
    fn download(&self, keep: Keep, progress: &Progress) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Text, recipients and threading headers of a message, from the archive or else downloaded;
    /// [`MailError::NotFound`] if it does not exist.
    fn body(&self, id: MessageId) -> impl Future<Output = Result<MessageBody, MailError>> + Send;

    /// The name and bytes of one attachment of a message (see [`Attachment::index`]).
    fn attachment(&self, id: MessageId, index: u32) -> impl Future<Output = Result<(String, Vec<u8>), MailError>> + Send;

    /// Keeps `raw` (RFC 5322 bytes carrying the header `X-BeaverMail-Draft: <id>`) in the Drafts folder,
    /// replacing any earlier copy of the draft `id`.
    fn save_draft(&self, id: &str, raw: Vec<u8>) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Removes every copy of the draft `id` from the Drafts folder.
    fn delete_draft(&self, id: &str) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Stores a copy of a sent message (RFC 5322 bytes) in the Sent folder, marked as read.
    fn save_sent(&self, raw: Vec<u8>) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Marks a message unread (`true`) or read (`false`).
    fn set_unread(&self, id: MessageId, unread: bool) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Stars a message (`true`) or takes its star off: `\Flagged`, as every client shows it.
    fn set_starred(&self, id: MessageId, starred: bool) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Sets label `keyword` on a message (`true`) or takes it off: an IMAP keyword on the server.
    fn set_label(&self, id: MessageId, keyword: &str, on: bool) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Pins a message to the top of its folder (`true`) or unpins it.
    fn set_pinned(&self, id: MessageId, pinned: bool) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Moves a message to the account's archive folder.
    fn archive(&self, id: MessageId) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Moves a message to the account's folder with `role`: to Spam when the user says it is spam, back
    /// to the Inbox when they say it is not.
    fn move_to(&self, id: MessageId, role: Role) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Moves a message to the account's folder named `folder`, which must exist.
    fn move_to_folder(&self, id: MessageId, folder: &str) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Creates folder `name` inside folder `parent`, or at the top with `None`; returns its full name on
    /// the server. The folder list is read again to show it.
    fn create_folder(&self, parent: Option<&str>, name: &str) -> impl Future<Output = Result<String, MailError>> + Send;

    /// Moves a message to the account's trash folder.
    fn trash(&self, id: MessageId) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Erases a message for good. Only messages already in the trash folder may be erased.
    fn erase(&self, id: MessageId) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Erases for good every message known here in the Trash or Spam folder (`role`); mail that
    /// arrived since the last sync stays. Returns how many were erased.
    fn empty(&self, role: Role) -> impl Future<Output = Result<usize, MailError>> + Send;
}

/// Sends mail for one account. Kept apart from [`MailSource`]: sending and reading use different servers.
pub trait MailSender {
    /// Delivers `message` to the account's outgoing server and returns the message as sent
    /// (RFC 5322 bytes), for a copy in the Sent folder. Addresses that do not parse are
    /// reported as [`MailError::Backend`] before anything is sent.
    fn send(&self, message: Outgoing) -> impl Future<Output = Result<Vec<u8>, MailError>> + Send;
}
