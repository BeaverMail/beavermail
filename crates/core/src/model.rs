// SPDX-License-Identifier: AGPL-3.0-only

//! Plain data types shared by every layer.

use serde::{Deserialize, Serialize};

/// Stable identifier of a message within one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MessageId(pub u64);

/// What a folder is for, when the UI must treat it specially.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Inbox,
    Starred,
    Drafts,
    Sent,
    Archive,
    Junk,
    Trash,
    Other,
}

/// A folder of messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    pub name: String,
    pub unread: u32,
    pub role: Role,
}

/// What the message list needs to show a row. The body is loaded separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub from: String,
    /// The sender's address, empty when the message has none.
    pub address: String,
    pub subject: String,
    pub preview: String,
    /// Display string for now; becomes a timestamp type once sync lands.
    pub received: String,
    pub unread: bool,
}

/// What opening a message needs beyond its summary: text, recipients and threading headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageBody {
    /// Plain text; the text conversion of the HTML when the message has no plain part.
    pub text: String,
    /// "Name <address>" or a bare address, as shown to the user.
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub reply_to: Vec<String>,
    /// Without angle brackets, as in the header value; empty when absent.
    pub message_id: String,
    pub references: Vec<String>,
}

/// Transport security of a mail server connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tls {
    /// Plain text. Only for a local test server.
    None,
    /// Plain connection upgraded with STARTTLS (usually IMAP 143, SMTP 587).
    StartTls,
    /// TLS from the first byte (usually IMAP 993, SMTP 465).
    Tls,
}

/// Where and how to reach one mail server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub tls: Tls,
}

/// A mail account's settings. The password is a secret and is never part of this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Label shown in the UI.
    pub name: String,
    /// Login name, shared by IMAP and SMTP. Often, but not always, the email address.
    pub user: String,
    /// The address mail is sent from. Empty in files written before this field existed.
    #[serde(default)]
    pub email: String,
    pub imap: Server,
    pub smtp: Server,
    /// Keep a copy of sent messages in the Sent folder. Turn off for providers that do it themselves.
    #[serde(default = "yes")]
    pub save_sent: bool,
}

fn yes() -> bool {
    true
}

impl Account {
    /// The address to send from: the configured one, else the login name.
    pub fn address(&self) -> &str {
        if self.email.trim().is_empty() { &self.user } else { self.email.trim() }
    }

    /// Stable identity of the account: also the key of its secret in the credential store.
    pub fn key(&self) -> String {
        format!("{}@{}", self.user, self.imap.host)
    }
}

/// A message to send. Addresses are free text until the sender parses them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// `Message-ID` of the message being answered, without angle brackets.
    pub in_reply_to: Option<String>,
    /// Message-IDs of the thread so far, oldest first, without angle brackets.
    pub references: Vec<String>,
}
