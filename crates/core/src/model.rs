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
    /// The server's separator between this folder and those inside it ("/" or "."), when known.
    pub delimiter: Option<char>,
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
    /// Pinned to the top of its folder: the `$Pinned` keyword on the server, or only here where the
    /// server takes no keywords of its own.
    pub pinned: bool,
    /// Flagged on the server (`\Flagged`: a star or flag set in any client); shown, it does not reorder.
    pub starred: bool,
    /// What the sender marked it as: 1 high, -1 low, 0 neither (`X-Priority`, `Importance`).
    pub priority: i8,
    /// Its labels, as IMAP keywords: the standard `$label1`… or names of the user's own.
    pub labels: Vec<String>,
    /// The message looks like it carries attachments (a `multipart/mixed` body).
    pub has_attachments: bool,
    /// The `Message-ID` header without angle brackets; empty when the message has none.
    pub message_id: String,
    /// The messages it answers, from `In-Reply-To` and `References`, without angle brackets: what
    /// groups it into a conversation.
    pub parents: Vec<String>,
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

/// A message found by a search, with the folder it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub message: MessageSummary,
    pub folder: String,
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
    /// The `Authentication-Results` headers (RFC 8601), topmost first. Who wrote each is untrusted until
    /// its `server` is checked against the account's own server.
    pub auth: Vec<AuthResult>,
    /// A header says the server's spam filter flagged the message.
    pub spam: bool,
    /// The rules the server's spam filter reports as hit (SpamAssassin names, such as `SPF_FAIL`).
    /// Anyone can write these headers, so they may only raise suspicion, never lower it.
    pub spam_tests: Vec<String>,
    /// The `List-Unsubscribe` links (RFC 2369), `https:` and `mailto:` only, in the sender's order.
    pub unsubscribe: Vec<String>,
    /// `List-Unsubscribe-Post: List-Unsubscribe=One-Click` (RFC 8058): an `https` link takes a POST, with no
    /// page to visit. Anyone can write the header, so it counts only for a sender the server verified.
    pub one_click: bool,
    /// The message is signed, and what its signature says.
    pub signature: Option<Signature>,
    /// The message was encrypted, and what became of it. Opened, the text and attachments above are
    /// the decrypted ones, which are never stored nor indexed.
    pub encryption: Option<Encryption>,
    /// Its calendar part (`text/calendar`, an invitation, a cancellation, an answer), as iCalendar text.
    pub calendar: Option<String>,
}

/// What became of an encrypted message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Encryption {
    /// Decrypted with one of the user's keys.
    Opened(SignedWith),
    /// None of the user's keys opens it.
    NoKey(SignedWith),
    Failed(SignedWith, String),
}

/// How a message was signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignedWith {
    Smime,
    OpenPgp,
}

/// What a signature says about the message carrying it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureCheck {
    /// It holds: made by `signer` (an address), vouched for by `by` (an authority, or a key).
    Valid { signer: String, by: String },
    /// It holds, but was made by someone other than the sender.
    NotSender { signer: String },
    /// It holds only with a key the message brought, while the sender used another key before.
    KeyChanged { signer: String },
    /// Made with a key not known here; `key` is its id.
    UnknownKey { key: String },
    /// The message was changed after it was signed.
    Changed,
    /// It cannot vouch for anyone: why.
    Untrusted(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub kind: SignedWith,
    pub check: SignatureCheck,
}

/// One `Authentication-Results` header: the server that wrote it and the checks it reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthResult {
    /// The `authserv-id`: the host that made the checks.
    pub server: String,
    pub checks: Vec<AuthCheck>,
}

/// "dkim=pass header.d=example.org": method and result in lower case, and the domain the check was about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthCheck {
    pub method: String,
    pub result: String,
    pub domain: String,
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
    /// The user's name as recipients see it beside the address, in From; may be empty. The account
    /// itself is shown by its address.
    pub name: String,
    /// Login name, for IMAP and, unless `smtp_user` says otherwise, SMTP. Often, but not always, the email address.
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
    /// Text added under new messages, replies and forwards; empty for none.
    #[serde(default)]
    pub signature: String,
    /// Signed in with this provider (OAuth 2.0) instead of a password: the secret kept for the
    /// account is then its refresh token.
    #[serde(default)]
    pub oauth: Option<OAuthProvider>,
    /// The outgoing server's own login, when it is not the incoming one's; its password is kept apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_user: Option<String>,
}

/// An address book synced over CardDAV: a source of its own, apart from the mail accounts (contacts
/// may live on another server than mail, or with no mail account at all).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressBook {
    /// As the server names it ("Personal", "Contacts").
    pub name: String,
    /// The address book's own URL (a collection).
    pub url: String,
    pub user: String,
    /// Contacts added in BeaverMail go to this one.
    #[serde(default)]
    pub default: bool,
    /// The server makes it and lets nobody write to it (people recently written to, its users):
    /// read, never written, never the default.
    #[serde(default)]
    pub read_only: bool,
}

/// A calendar: one synced over CalDAV, a source of its own as address books are, or the one kept on
/// this computer only ([`CalendarSource::LOCAL`] as its URL).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarSource {
    /// As the server names it ("Work", "Family").
    pub name: String,
    /// The calendar's own URL (a collection).
    pub url: String,
    #[serde(default)]
    pub user: String,
    /// `#RRGGBB`, from the server or chosen here; empty for the app's own.
    #[serde(default)]
    pub color: String,
    /// Its events show in the views.
    #[serde(default = "shown")]
    pub shown: bool,
    /// Accepted invitations go to this one.
    #[serde(default)]
    pub default: bool,
    /// Nothing can be written there (shared read-only, a subscription).
    #[serde(default)]
    pub read_only: bool,
}

impl CalendarSource {
    /// The URL that stands for the calendar on this computer.
    pub const LOCAL: &'static str = "local:";

    pub fn is_local(&self) -> bool {
        self.url == Self::LOCAL
    }
}

fn shown() -> bool {
    true
}

/// "Ada Lovelace <ada@x.org>" as an item of an address list: the name quoted when it has a character
/// that would cut it short (a comma, a colon, parentheses, an @…), the address alone when there is no
/// name or the name is the address.
pub fn address_text(name: &str, address: &str) -> String {
    let (name, address) = (name.trim(), address.trim());
    if name.is_empty() || name.eq_ignore_ascii_case(address) {
        return address.to_owned();
    }
    if name.contains(['(', ')', '<', '>', '[', ']', ':', ';', '@', '\\', ',', '.', '"']) {
        format!("\"{}\" <{address}>", name.replace(['"', '\\'], ""))
    } else {
        format!("{name} <{address}>")
    }
}

/// Who an account signs in with, when it is not a password.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    Google,
    Microsoft,
}

impl OAuthProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Microsoft => "Microsoft",
        }
    }
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

impl Account {
    /// The address to send from: the configured one, else the login name.
    pub fn address(&self) -> &str {
        if self.email.trim().is_empty() { &self.user } else { self.email.trim() }
    }

    /// The name recipients see beside the address: none when `name` is empty or only an address or
    /// the login (what earlier versions filled in when it was left empty).
    pub fn sender_name(&self) -> Option<&str> {
        let name = self.name.trim();
        (!name.is_empty() && !name.contains('@') && !name.eq_ignore_ascii_case(self.user.trim())).then_some(name)
    }

    /// The From of messages sent from this account: "\"Ada Lovelace\" <ada@example.org>", or the address alone.
    pub fn sender(&self) -> String {
        match self.sender_name() {
            // Quoted, so a comma or a dot in the name stays part of it; quotes inside would end it early.
            Some(name) => format!("\"{}\" <{}>", name.replace(['"', '\\'], ""), self.address()),
            None => self.address().to_owned(),
        }
    }

    /// The login for the outgoing server.
    pub fn smtp_login(&self) -> &str {
        self.smtp_user.as_deref().unwrap_or(&self.user)
    }

    /// Stable identity of the account: also the key of its secret in the credential store.
    pub fn key(&self) -> String {
        format!("{}@{}", self.user, self.imap.host)
    }
}

/// A file to send with a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingFile {
    /// The name the recipient sees.
    pub name: String,
    /// "type/subtype", e.g. "application/pdf".
    pub mime: String,
    pub data: Vec<u8>,
}

/// A message to send. Addresses are free text until the sender parses them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// Blind copies: they get the message, and no other recipient sees them.
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// `Message-ID` of the message being answered, without angle brackets.
    pub in_reply_to: Option<String>,
    /// Message-IDs of the thread so far, oldest first, without angle brackets.
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingFile>,
    /// `body` is Markdown: it is sent as is for the plain-text part, plus a rendered HTML part.
    pub rich: bool,
    /// The sender's OpenPGP public key, handed out in an `Autocrypt:` header.
    pub autocrypt: Option<Vec<u8>>,
    /// Encrypt with OpenPGP before sending.
    pub seal: Option<Seal>,
    /// An iTIP message (RFC 6047) sent beside the text, as `text/calendar` with the method it says
    /// (an answer to an invitation).
    pub calendar: Option<String>,
}

/// How to encrypt a message, signed inside by the sender, who also gets a copy they can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seal {
    /// With the sender's OpenPGP secret key; to the keys of To and Cc, and of Bcc without naming them.
    OpenPgp { secret: Vec<u8>, to: Vec<Vec<u8>>, bcc: Vec<Vec<u8>> },
    /// With the sender's certificate and key (as kept); to every recipient's certificate.
    Smime { own: Vec<u8>, to: Vec<Vec<u8>> },
}

/// How an incoming message is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    /// The plain-text part, or a text conversion of the HTML.
    #[default]
    Plain,
    /// A cleaned-up subset of the HTML: text structure and links, no images, styles or remote content.
    Simplified,
    /// The HTML as designed, drawn by a layout engine; nothing is loaded from a server.
    Original,
}

/// User preferences, stored apart from the accounts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// How messages are shown unless a sender has its own setting.
    #[serde(default)]
    pub display: DisplayMode,
    /// Per-sender overrides, by lower-case address.
    #[serde(default)]
    pub sender_display: std::collections::BTreeMap<String, DisplayMode>,
    /// Write messages in Markdown and send them as plain text plus HTML.
    #[serde(default = "yes")]
    pub rich_outgoing: bool,
    /// Senders (lower-case addresses) whose messages may load remote images and styles in the original view.
    #[serde(default)]
    pub remote_senders: std::collections::BTreeSet<String>,
    /// Seconds a message waits after Send, so it can be taken back; 0 sends at once.
    #[serde(default = "undo_secs")]
    pub undo_send_secs: u32,
    /// Hold suspicious mail from new senders apart from the Inbox.
    #[serde(default = "yes")]
    pub hold_suspicious: bool,
    /// Senders the user marked: a lower-case address, or `@domain` for a whole organisation; `true` not
    /// spam, `false` spam.
    /// shortcut: the marks stay on this computer; the messages marked move to and from the server's Spam
    /// folder, but a new message from a marked sender is only held here. A Sieve rule (or the server's
    /// own filter learning from the Spam folder) would hold it on every device.
    #[serde(default)]
    pub marked: std::collections::BTreeMap<String, bool>,
    /// Show a conversation as one row in the list, its messages together in the reader.
    #[serde(default = "yes")]
    pub threads: bool,
    /// The message shows beside the list; off, it opens in a tab.
    #[serde(default = "yes")]
    pub reading_pane: bool,
    /// Accounts (by [`Account::key`]) folded in the sidebar, so they stay folded at the next start, and
    /// the folders opened (folders start folded).
    #[serde(default)]
    pub folded: std::collections::BTreeSet<String>,
    /// Link parameters removed before a link is opened or copied, besides the known tracking ones:
    /// names separated by spaces or commas; one ending in `*` removes every parameter starting so.
    #[serde(default)]
    pub link_params: String,
    /// The address books synced over CardDAV; their passwords are in the credential store.
    #[serde(default)]
    pub address_books: Vec<AddressBook>,
    /// The calendars, synced over CalDAV or on this computer; passwords in the credential store.
    #[serde(default)]
    pub calendars: Vec<CalendarSource>,
}

fn yes() -> bool {
    true
}

fn undo_secs() -> u32 {
    10
}

impl Default for Settings {
    fn default() -> Self {
        Self { display: DisplayMode::default(), sender_display: Default::default(), rich_outgoing: true, undo_send_secs: undo_secs(), remote_senders: Default::default(), hold_suspicious: true, marked: Default::default(), threads: true, reading_pane: true, folded: Default::default(), link_params: String::new(), address_books: Vec::new(), calendars: Vec::new() }
    }
}

impl Settings {
    /// The display mode for a message from `address`.
    pub fn mode_for(&self, address: &str) -> DisplayMode {
        self.sender_display.get(&address.trim().to_lowercase()).copied().unwrap_or(self.display)
    }

    pub fn allows_remote(&self, address: &str) -> bool {
        self.remote_senders.contains(&address.trim().to_lowercase())
    }

    /// Remembers that messages from `address` may (or may not any more) load remote content.
    pub fn set_remote(&mut self, address: &str, allow: bool) {
        let key = address.trim().to_lowercase();
        if key.is_empty() {
            return;
        }
        if allow {
            self.remote_senders.insert(key);
        } else {
            self.remote_senders.remove(&key);
        }
    }

    /// How the user marked `address` (or `@domain`): `Some(true)` not spam, `Some(false)` spam.
    pub fn mark_of(&self, address: &str) -> Option<bool> {
        self.marked.get(&address.trim().to_lowercase()).copied()
    }

    /// Marks `address` (or `@domain`) as not spam (`Some(true)`), spam (`Some(false)`), or forgets the mark.
    pub fn set_mark(&mut self, address: &str, not_spam: Option<bool>) {
        let key = address.trim().to_lowercase();
        match not_spam {
            _ if key.is_empty() => {}
            Some(not_spam) => {
                self.marked.insert(key, not_spam);
            }
            None => {
                self.marked.remove(&key);
            }
        }
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
    fn mail_goes_out_under_the_users_name_unless_it_is_only_the_login() {
        let server = Server { host: "imap.x.org".into(), port: 993, tls: Tls::Tls };
        let account = |name: &str| Account { name: name.into(), user: "ada".into(), email: "ada@x.org".into(), imap: server.clone(), smtp: server.clone(), save_sent: true, keep: Keep::default(), signature: String::new(), oauth: None, smtp_user: None };
        assert_eq!(account("Ada Lovelace").sender(), "\"Ada Lovelace\" <ada@x.org>");
        assert_eq!(account("Lovelace, Ada").sender(), "\"Lovelace, Ada\" <ada@x.org>", "quoted: the comma is part of the name");
        assert_eq!(account("Ada \"the\" Countess").sender(), "\"Ada the Countess\" <ada@x.org>");
        for none in ["", "  ", "ada@x.org", "ADA"] {
            assert_eq!(account(none).sender(), "ada@x.org", "{none:?}: what older versions filled in, or nothing");
        }
    }

    #[test]
    fn sender_overrides_beat_the_default_and_clear_themselves() {
        let mut settings = Settings::default();
        assert_eq!(settings.mode_for("a@x.org"), DisplayMode::Plain);
        settings.set_sender_mode(" A@X.org ", DisplayMode::Simplified);
        assert_eq!(settings.mode_for("a@x.org"), DisplayMode::Simplified);
        assert_eq!(settings.mode_for("b@x.org"), DisplayMode::Plain);
        settings.set_sender_mode("a@x.org", DisplayMode::Plain);
        assert!(settings.sender_display.is_empty());
        assert!(!settings.allows_remote("a@x.org"));
        settings.set_remote(" A@X.org ", true);
        assert!(settings.allows_remote("a@x.org"));
        settings.set_remote("a@x.org", false);
        assert!(settings.remote_senders.is_empty());
        settings.set_mark(" Spam@X.org", Some(false));
        assert_eq!(settings.mark_of("spam@x.org"), Some(false));
        settings.set_mark("spam@x.org", None);
        assert_eq!(settings.mark_of("spam@x.org"), None);
    }
}
