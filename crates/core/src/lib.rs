// SPDX-License-Identifier: AGPL-3.0-only

//! Domain model and the boundary between the UI and mail sources.
//!
//! This crate must not depend on any GUI or network code. Protocol backends (JMAP, IMAP,
//! POP3) implement [`MailSource`] in their own crates.

pub mod model;
pub mod sample;

pub use model::{Mailbox, MessageId, MessageSummary};

/// Read access to one account's mail. The UI talks only to this trait.
pub trait MailSource {
    /// Mailboxes (folders) of the account, in display order.
    fn mailboxes(&self) -> Vec<Mailbox>;

    /// Summaries of the messages in `mailbox`, newest first.
    fn messages(&self, mailbox: &str) -> Vec<MessageSummary>;

    /// Plain-text body of a message, if it exists.
    fn body(&self, id: MessageId) -> Option<String>;
}
