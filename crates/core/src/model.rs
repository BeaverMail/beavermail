// SPDX-License-Identifier: AGPL-3.0-only

//! Plain data types shared by every layer.

/// Stable identifier of a message within one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MessageId(pub u64);

/// A folder of messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    pub name: String,
    pub unread: u32,
}

/// What the message list needs to show a row. The body is loaded separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub from: String,
    pub subject: String,
    pub preview: String,
    /// Display string for now; becomes a timestamp type once sync lands.
    pub received: String,
    pub unread: bool,
}
