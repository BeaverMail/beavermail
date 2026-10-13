// SPDX-License-Identifier: AGPL-3.0-only

//! Domain model and the boundary between the UI and mail sources.
//!
//! This crate must not depend on any GUI or network code. Protocol backends (JMAP, IMAP,
//! POP3) implement [`MailSource`] in their own crates.

pub mod model;
pub mod sample;

pub use model::{Account, Attachment, DisplayMode, Keep, Mailbox, MessageBody, MessageId, MessageSummary, Outgoing, OutgoingFile, Progress, Role, Server, Settings, SyncReport, Tls};

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
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Auth(why) => write!(f, "authentication failed: {why}"),
            Self::Backend(why) => write!(f, "mail backend error: {why}"),
        }
    }
}

impl std::error::Error for MailError {}

/// Access to one account's mail. The UI talks only to this trait.
///
/// Methods return `Send` futures so a backend can be driven from any executor;
/// implementors can still write plain `async fn`.
pub trait MailSource {
    /// Mailboxes (folders) as the local archive knows them, without touching the network.
    fn cached_mailboxes(&self) -> impl Future<Output = Result<Vec<Mailbox>, MailError>> + Send;

    /// Asks the server for the mailboxes, remembers them locally and returns them.
    fn mailboxes(&self) -> impl Future<Output = Result<Vec<Mailbox>, MailError>> + Send;

    /// Summaries of the messages in `mailbox` from the local archive, newest first. Instant and
    /// available offline; [`MailSource::sync`] is what brings the archive up to date.
    fn messages(&self, mailbox: &str) -> impl Future<Output = Result<Vec<MessageSummary>, MailError>> + Send;

    /// Brings `mailbox` in the local archive up to date: new messages, changed flags, deleted
    /// messages, and one more chunk of older history. Call again while
    /// [`SyncReport::remaining`] is not zero.
    fn sync(&self, mailbox: &str) -> impl Future<Output = Result<SyncReport, MailError>> + Send;

    /// Downloads the messages `keep` asks for, newest first, folder by folder, after synchronising
    /// the folders far enough back. Reports through `progress`, waits while it is paused and returns
    /// early when it is cancelled.
    fn download(&self, keep: Keep, progress: &Progress) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Text, recipients and threading headers of a message, from the archive or else downloaded;
    /// [`MailError::NotFound`] if it does not exist.
    fn body(&self, id: MessageId) -> impl Future<Output = Result<MessageBody, MailError>> + Send;

    /// The name and bytes of one attachment of a message (see [`Attachment::index`]).
    fn attachment(&self, id: MessageId, index: u32) -> impl Future<Output = Result<(String, Vec<u8>), MailError>> + Send;

    /// Stores a copy of a sent message (RFC 5322 bytes) in the Sent folder, marked as read.
    fn save_sent(&self, raw: Vec<u8>) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Marks a message unread (`true`) or read (`false`).
    fn set_unread(&self, id: MessageId, unread: bool) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Moves a message to the account's archive folder.
    fn archive(&self, id: MessageId) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Moves a message to the account's trash folder.
    fn trash(&self, id: MessageId) -> impl Future<Output = Result<(), MailError>> + Send;

    /// Erases a message for good. Only messages already in the trash folder may be erased.
    fn erase(&self, id: MessageId) -> impl Future<Output = Result<(), MailError>> + Send;
}

/// Sends mail for one account. Kept apart from [`MailSource`]: sending and reading use different servers.
pub trait MailSender {
    /// Delivers `message` to the account's outgoing server and returns the message as sent
    /// (RFC 5322 bytes), for a copy in the Sent folder. Addresses that do not parse are
    /// reported as [`MailError::Backend`] before anything is sent.
    fn send(&self, message: Outgoing) -> impl Future<Output = Result<Vec<u8>, MailError>> + Send;
}
