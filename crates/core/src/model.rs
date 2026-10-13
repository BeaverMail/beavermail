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
    /// Messages in the folder on the server, as of the last time it was listed; 0 until then.
    pub total: u32,
    /// Messages of the folder whose file is stored on this computer.
    pub stored: u32,
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
    /// Display text of the date.
    pub received: String,
    /// Seconds since the Unix epoch, for ordering; 0 when the message has no valid date.
    pub timestamp: i64,
    pub unread: bool,
    /// The message looks like it carries attachments (a `multipart/mixed` body).
    pub has_attachments: bool,
}

/// What one synchronisation of a folder did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Older messages on the server that are not in the archive yet (an estimate).
    pub remaining: u32,
}

/// A file attached to a message. The bytes stay in the message file until asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// Position among the message's attachments; the key to fetch the bytes.
    pub index: u32,
    /// The name the sender gave it, or a made-up one. Untrusted: it is a hint, not a path.
    pub name: String,
    pub mime: String,
    pub size: u64,
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
    /// The `text/html` part, only when the message really has one. Untrusted: never run it.
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
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
    /// How much of the mailbox is downloaded for offline use.
    #[serde(default)]
    pub keep: Keep,
}

/// How much mail is kept on this computer. Headers and flags are always synchronised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Keep {
    /// Every message.
    All,
    /// Messages of the last 12 months.
    Year,
    /// Messages of the last 30 days.
    #[default]
    Month,
    /// Only messages that were opened.
    Opened,
}

impl Keep {
    /// The oldest date (seconds since the Unix epoch) to download at `now`, or `None` for nothing.
    pub fn since(self, now: i64) -> Option<i64> {
        const DAY: i64 = 86_400;
        match self {
            Self::All => Some(i64::MIN),
            Self::Year => Some(now - 365 * DAY),
            Self::Month => Some(now - 30 * DAY),
            Self::Opened => None,
        }
    }
}

/// Progress of the background download of one account, shared with the window that shows it.
#[derive(Debug, Default)]
pub struct Progress {
    /// Messages to download, known once the folders are synchronised.
    pub total: std::sync::atomic::AtomicU32,
    pub done: std::sync::atomic::AtomicU32,
    /// A download is running (set by whoever starts it).
    pub running: std::sync::atomic::AtomicBool,
    /// The user paused it; the worker waits.
    pub paused: std::sync::atomic::AtomicBool,
    /// Stop as soon as possible (the account was removed or changed).
    pub cancel: std::sync::atomic::AtomicBool,
    /// What it is doing now, for the status bar.
    pub activity: std::sync::Mutex<String>,
}

impl Progress {
    pub fn set_activity(&self, text: impl Into<String>) {
        *self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = text.into();
    }

    pub fn activity(&self) -> String {
        self.activity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
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

/// How an incoming message is shown (PRD D9). Original HTML is not implemented yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    /// The plain-text part, or a text conversion of the HTML.
    #[default]
    Plain,
    /// A cleaned-up subset of the HTML: text structure and links, no images, styles or remote content.
    Simplified,
}

/// User preferences, stored apart from the accounts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// How messages are shown unless a sender has its own setting.
    #[serde(default)]
    pub display: DisplayMode,
    /// Per-sender overrides, by lower-case address.
    #[serde(default)]
    pub sender_display: std::collections::BTreeMap<String, DisplayMode>,
}

impl Settings {
    /// The display mode for a message from `address`.
    pub fn mode_for(&self, address: &str) -> DisplayMode {
        self.sender_display.get(&address.trim().to_lowercase()).copied().unwrap_or(self.display)
    }

    /// Remembers `mode` for `address`; a mode equal to the default clears the override.
    pub fn set_sender_mode(&mut self, address: &str, mode: DisplayMode) {
        let key = address.trim().to_lowercase();
        if key.is_empty() {
            return;
        }
        if mode == self.display {
            self.sender_display.remove(&key);
        } else {
            self.sender_display.insert(key, mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_overrides_beat_the_default_and_clear_themselves() {
        let mut settings = Settings::default();
        assert_eq!(settings.mode_for("a@x.org"), DisplayMode::Plain);
        settings.set_sender_mode(" A@X.org ", DisplayMode::Simplified);
        assert_eq!(settings.mode_for("a@x.org"), DisplayMode::Simplified);
        assert_eq!(settings.mode_for("b@x.org"), DisplayMode::Plain);
        settings.set_sender_mode("a@x.org", DisplayMode::Plain);
        assert!(settings.sender_display.is_empty());
    }
}
