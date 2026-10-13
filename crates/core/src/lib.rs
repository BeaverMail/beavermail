// SPDX-License-Identifier: AGPL-3.0-only

//! Domain model and the boundary between the UI and mail sources.
//!
//! This crate must not depend on any GUI or network code. Protocol backends (JMAP, IMAP,
//! POP3) implement [`MailSource`] in their own crates.

pub mod model;
pub mod sample;

pub use model::{Account, Mailbox, MessageId, MessageSummary, Outgoing, Role, Server, Tls};

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
    /// Mailboxes (folders) of the account, in display order.
    fn mailboxes(&self) -> impl Future<Output = Result<Vec<Mailbox>, MailError>> + Send;

    /// Summaries of the messages in `mailbox`, newest first.
    fn messages(&self, mailbox: &str) -> impl Future<Output = Result<Vec<MessageSummary>, MailError>> + Send;

    /// Plain-text body of a message; [`MailError::NotFound`] if it does not exist.
    fn body(&self, id: MessageId) -> impl Future<Output = Result<String, MailError>> + Send;

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
    /// Delivers `message` to the account's outgoing server. Addresses that do not parse are
    /// reported as [`MailError::Backend`] before anything is sent.
    fn send(&self, message: Outgoing) -> impl Future<Output = Result<(), MailError>> + Send;
}
