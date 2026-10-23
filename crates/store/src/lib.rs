// SPDX-License-Identifier: AGPL-3.0-only

//! The local mail archive: folders, message headers, bodies and a full-text index in SQLite.
//!
//! Everything the UI shows comes from here first, so the window opens instantly and works
//! offline; the protocol backends only keep it up to date. Rows are keyed by account, folder and
//! IMAP UID, and a [`MessageId`] is the row id, which stays valid across sessions.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use beavermail_core::{Hit, MailError, MessageId, MessageSummary, Role};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS folders (
    account TEXT NOT NULL,
    name TEXT NOT NULL,
    role INTEGER NOT NULL DEFAULT 7,
    uidvalidity INTEGER,
    -- Messages with a UID below this have not been downloaded yet; NULL until the first sync.
    floor_uid INTEGER,
    PRIMARY KEY (account, name)
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    folder TEXT NOT NULL,
    uid INTEGER NOT NULL,
    from_name TEXT NOT NULL,
    from_addr TEXT NOT NULL,
    subject TEXT NOT NULL,
    preview TEXT NOT NULL DEFAULT '',
    received TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    unread INTEGER NOT NULL,
    message_id TEXT NOT NULL DEFAULT '',
    in_reply_to TEXT NOT NULL DEFAULT '',
    refs TEXT NOT NULL DEFAULT '',
    UNIQUE (account, folder, uid)
);
CREATE INDEX IF NOT EXISTS messages_list ON messages (account, folder, timestamp DESC, uid DESC);
CREATE INDEX IF NOT EXISTS messages_thread ON messages (account, message_id);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
    subject, from_name, from_addr, preview,
    content = 'messages', content_rowid = 'id', tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, subject, from_name, from_addr, preview)
    VALUES (new.id, new.subject, new.from_name, new.from_addr, new.preview);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, subject, from_name, from_addr, preview)
    VALUES ('delete', old.id, old.subject, old.from_name, old.from_addr, old.preview);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE OF subject, from_name, from_addr, preview ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, subject, from_name, from_addr, preview)
    VALUES ('delete', old.id, old.subject, old.from_name, old.from_addr, old.preview);
    INSERT INTO messages_fts (rowid, subject, from_name, from_addr, preview)
    VALUES (new.id, new.subject, new.from_name, new.from_addr, new.preview);
END;
";

pub struct Store {
    inner: Mutex<Inner>,
}

/// The open database and what is known about how it is kept on disk.
struct Inner {
    conn: Connection,
    /// The database file; `None` for one in memory.
    path: Option<PathBuf>,
    /// The key the file is encrypted with; `None` when it is plain.
    key: Option<[u8; 32]>,
    /// A stand-in until the key is available (a passphrase not entered yet): empty, and in memory.
    locked: bool,
}

impl std::ops::Deref for Inner {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl std::ops::DerefMut for Inner {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// A message header as the sync reads it from the server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub uid: u32,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub received: String,
    pub timestamp: i64,
    pub unread: bool,
    /// The `$Pinned` keyword: kept at the top of its folder.
    pub pinned: bool,
    /// `\Flagged`: a star or flag from any client.
    pub starred: bool,
    /// 1 high, -1 low, 0 neither, as the sender marked it.
    pub priority: i8,
    /// Labels from other clients, separated by spaces (keywords have none).
    pub labels: String,
    pub message_id: String,
    pub in_reply_to: String,
    /// Message-IDs separated by spaces.
    pub references: String,
    /// Size of the whole message in bytes; 0 when the server did not say.
    pub size: u32,
    pub has_attachments: bool,
    /// The To and Cc addresses, lower case, separated by spaces.
    pub recipients: String,
    /// The header fields as fetched, for the checks that need no body (sender, Reply-To, the server's verdict).
    pub raw: Vec<u8>,
}

/// The flags of a cached message the server can change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub unread: bool,
    pub pinned: bool,
    pub starred: bool,
    /// Separated by spaces.
    pub labels: String,
}

/// A message whose file has not been downloaded yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub uid: u32,
    pub size: u32,
}

/// Someone to suggest in an address field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub name: String,
    /// Lower case.
    pub address: String,
    /// The name and address as [`fold`] makes them, to search.
    pub folded: String,
}

impl Contact {
    pub fn new(name: &str, address: &str) -> Self {
        Self { name: name.to_owned(), address: address.to_lowercase(), folded: fold(&format!("{name} {address}")) }
    }

    /// Whether every word of `query` starts a word of the name or address, accents and case aside.
    pub fn matches(&self, query: &str) -> bool {
        let words = fold(query);
        !words.trim().is_empty() && words.split_whitespace().all(|w| self.folded.contains(&format!(" {w}")))
    }
}

/// Where a card of an address book stands against the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardState {
    /// As on the server.
    Synced,
    /// Changed here, not sent yet.
    Changed,
    /// Made here, not on the server yet.
    New,
    /// Deleted here, still on the server.
    Deleted,
}

impl CardState {
    fn from_int(n: i64) -> Self {
        match n {
            1 => Self::Changed,
            2 => Self::New,
            3 => Self::Deleted,
            _ => Self::Synced,
        }
    }

    fn to_int(self) -> i64 {
        match self {
            Self::Synced => 0,
            Self::Changed => 1,
            Self::New => 2,
            Self::Deleted => 3,
        }
    }
}

/// A card of an address book synced over CardDAV.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCard {
    pub id: i64,
    /// The address book's URL.
    pub book: String,
    pub href: String,
    /// The server's version of it; empty for one not sent yet.
    pub etag: String,
    pub vcard: String,
    pub name: String,
    /// Lower case.
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub note: String,
    pub state: CardState,
}

/// An event of a calendar (a calendar object: an event with its changed occurrences) as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub id: i64,
    /// The calendar's URL.
    pub calendar: String,
    pub href: String,
    /// The server's version of it; empty for one not sent yet.
    pub etag: String,
    /// The iCalendar text.
    pub ics: String,
    pub state: CardState,
}

/// When a stored event happens, worked out by whoever stores it, so a view asks only for the events
/// in its range: its first start, its `UID`, and when its last occurrence ends (`None`: it repeats
/// with no end).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventSpan {
    pub uid: String,
    pub first: i64,
    pub last: Option<i64>,
}

/// What a card shows, read from its vCard by whoever stores it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardFields {
    pub name: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub note: String,
}

/// What the archive knows of someone, for their card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContactCard {
    pub name: String,
    pub address: String,
    /// In the address book the user keeps, not only suggested from mail.
    pub saved: bool,
    pub notes: String,
    /// Messages the user sent them, from every account, and when the last one went.
    pub sent: u32,
    pub last_sent: Option<i64>,
}

/// Lower case without accents, each run of other characters one space, and a space in front
/// (" nicolo rossi n rossi x org"): a word starts after a space.
pub fn fold(text: &str) -> String {
    let mut out = String::from(" ");
    for c in text.to_lowercase().chars() {
        let plain = match c {
            'à'..='å' | 'ā' | 'ă' | 'ą' => "a",
            'æ' => "ae",
            'ç' | 'ć' | 'č' => "c",
            'ð' | 'ď' | 'đ' => "d",
            'è'..='ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
            'ì'..='ï' | 'ī' | 'į' | 'ı' => "i",
            'ł' | 'ľ' => "l",
            'ñ' | 'ń' | 'ň' => "n",
            'ò'..='ö' | 'ø' | 'ō' | 'ő' => "o",
            'œ' => "oe",
            'ř' => "r",
            'ß' => "ss",
            'ś' | 'š' | 'ş' => "s",
            'ť' | 'ţ' => "t",
            'ù'..='ü' | 'ū' | 'ů' | 'ű' => "u",
            'ý' | 'ÿ' => "y",
            'ź' | 'ż' | 'ž' => "z",
            c if c.is_alphanumeric() => {
                out.push(c);
                continue;
            }
            _ => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
                continue;
            }
        };
        out.push_str(plain);
    }
    out.trim_end().to_owned()
}

/// Counts a message the user sent for each of its recipients, once: by its Message-ID (`key`), so a
/// folder downloaded again, or the same message in two Sent folders, counts nothing twice.
fn learn_sent(conn: &Connection, account: &str, key: &str, recipients: &str, at: i64) -> rusqlite::Result<()> {
    if recipients.trim().is_empty() || conn.execute("INSERT OR IGNORE INTO contacts_learned (account, message) VALUES (?1, ?2)", params![account, key])? == 0 {
        return Ok(());
    }
    let mut seen = std::collections::HashSet::new();
    for address in recipients.split_whitespace().filter(|a| a.contains('@') && seen.insert(*a)) {
        if conn.execute("INSERT OR IGNORE INTO contacts (address, folded) VALUES (?1, ?2)", params![address, fold(address)])? == 1 {
            // New: named as the latest of their own mail already here names them, Spam aside.
            let name: Option<String> = conn
                .query_row(
                    "SELECT m.from_name FROM messages m LEFT JOIN folders f ON f.account = m.account AND f.name = m.folder
                     WHERE lower(m.from_addr) = ?1 AND m.from_name != '' AND COALESCE(f.role, ?2) != ?3 ORDER BY m.timestamp DESC LIMIT 1",
                    params![address, role_to_int(Role::Other), role_to_int(Role::Junk)],
                    |r| r.get(0),
                )
                .optional()?;
            learn_name(conn, address, &name.unwrap_or_default())?;
        }
        conn.execute(
            "INSERT INTO contact_accounts (address, account, sent, last) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT (address, account) DO UPDATE SET sent = sent + 1, last = MAX(last, excluded.last)",
            params![address, account, at],
        )?;
    }
    Ok(())
}

/// Names a contact learned from mail as their own mail names them; a saved contact keeps its name.
/// shortcut: the last message read wins, which while history downloads may be an older one; keep the
/// newest name if that shows.
fn learn_name(conn: &Connection, address: &str, name: &str) -> rusqlite::Result<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE contacts SET name = ?2, folded = ?3 WHERE address = ?1 AND saved = 0 AND name != ?2",
        params![address.to_lowercase(), name.trim(), fold(&format!("{name} {address}"))],
    )
    .map(|_| ())
}

/// The key a sent message is learned under: its Message-ID, or where it is when it has none.
fn learn_key(message_id: &str, folder: &str, uid: u32) -> String {
    if message_id.is_empty() { format!("{folder}\0{uid}") } else { message_id.to_owned() }
}

/// What the archive remembers about a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderState {
    pub uidvalidity: Option<u32>,
    pub floor_uid: Option<u32>,
}

/// A folder with the counts the sidebar shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRow {
    pub name: String,
    pub role: Role,
    pub unread: u32,
    /// Messages on the server at the last listing; the number of cached ones before that.
    pub total: u32,
    /// Messages whose file is stored.
    pub stored: u32,
    /// The server's separator between this folder and those inside it; `None` until it lists it again.
    pub delimiter: Option<char>,
}

/// Where a message lives on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub account: String,
    pub folder: String,
    pub uid: u32,
}

fn db(e: impl std::fmt::Display) -> MailError {
    MailError::Backend(format!("local archive: {e}"))
}

fn role_to_int(role: Role) -> i64 {
    match role {
        Role::Inbox => 0,
        Role::Starred => 1,
        Role::Drafts => 2,
        Role::Sent => 3,
        Role::Archive => 4,
        Role::Junk => 5,
        Role::Trash => 6,
        Role::Other => 7,
    }
}

fn role_from_int(n: i64) -> Role {
    match n {
        0 => Role::Inbox,
        1 => Role::Starred,
        2 => Role::Drafts,
        3 => Role::Sent,
        4 => Role::Archive,
        5 => Role::Junk,
        6 => Role::Trash,
        _ => Role::Other,
    }
}

/// The date as the reader expects it: in this computer's time zone, not the sender's.
fn local_time(timestamp: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(timestamp, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
}

fn summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
    let timestamp: i64 = row.get(6)?;
    Ok(MessageSummary {
        id: MessageId(row.get::<_, i64>(0)? as u64),
        from: row.get(1)?,
        address: row.get(2)?,
        subject: row.get(3)?,
        preview: row.get(4)?,
        // The stored text is the sender's clock; it stays only for a message without a valid date.
        received: if timestamp == 0 { row.get(5)? } else { local_time(timestamp).map_or_else(|| row.get(5), Ok)? },
        timestamp,
        unread: row.get::<_, i64>(7)? != 0,
        has_attachments: row.get::<_, i64>(8)? != 0,
        pinned: row.get::<_, i64>(12)? != 0,
        starred: row.get::<_, i64>(13)? != 0,
        priority: row.get::<_, i64>(14)?.clamp(-1, 1) as i8,
        labels: row.get::<_, String>(15)?.split_whitespace().map(str::to_owned).collect(),
        message_id: row.get(9)?,
        parents: {
            let (in_reply_to, refs): (String, String) = (row.get(10)?, row.get(11)?);
            let mut parents: Vec<String> = refs.split_whitespace().chain(in_reply_to.split_whitespace()).map(str::to_owned).collect();
            parents.dedup();
            parents
        },
    })
}

/// A search as the user typed it, split into what it asks for.
#[derive(Default, Debug, PartialEq, Eq)]
struct Query {
    words: Vec<String>,
    from: Vec<String>,
    /// Parts of a To or Cc address.
    to: Vec<String>,
    subject: Vec<String>,
    /// Received on or after this time, and before this one (seconds), from `after:` and `before:`.
    after: Option<i64>,
    before: Option<i64>,
    folders: Vec<String>,
    unread: bool,
    attachment: bool,
}

impl Query {
    fn parse(text: &str) -> Self {
        let mut query = Self::default();
        for token in tokens(text) {
            match token.split_once(':') {
                Some(("from", v)) if !v.is_empty() => query.from.push(v.to_owned()),
                Some(("to", v)) if !v.is_empty() => query.to.push(v.to_lowercase()),
                Some(("after", v)) if day_start(v).is_some() => query.after = day_start(v),
                Some(("before", v)) if day_start(v).is_some() => query.before = day_start(v),
                Some(("subject", v)) if !v.is_empty() => query.subject.push(v.to_owned()),
                Some(("in", v)) if v.eq_ignore_ascii_case("all") => {}
                Some(("in", v)) if !v.is_empty() => query.folders.push(v.to_owned()),
                Some(("is", v)) if v.eq_ignore_ascii_case("unread") => query.unread = true,
                Some(("has", v)) if v.eq_ignore_ascii_case("attachment") => query.attachment = true,
                _ => query.words.push(token),
            }
        }
        query
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The first moment, on this computer's clock, of a day ("2026-10-01"), a month ("2026-10") or a year.
fn day_start(text: &str) -> Option<i64> {
    let full = match text.matches('-').count() {
        0 => format!("{text}-01-01"),
        1 => format!("{text}-01"),
        _ => text.to_owned(),
    };
    let day = chrono::NaiveDate::parse_from_str(&full, "%Y-%m-%d").ok()?;
    Some(day.and_hms_opt(0, 0, 0)?.and_local_timezone(chrono::Local).earliest()?.timestamp())
}

/// Splits on spaces; a double-quoted run stays one token (without its quotes).
fn tokens(text: &str) -> Vec<String> {
    let (mut out, mut current, mut quoted) = (Vec::new(), String::new(), false);
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One search term as an FTS5 phrase, quoted so that it cannot be taken for syntax. A single word also
/// matches as a prefix ("budg" finds "budget").
fn fts_term(word: &str) -> String {
    let clean = word.replace('"', " ");
    let clean = clean.trim();
    if clean.contains(char::is_whitespace) { format!("\"{clean}\"") } else { format!("\"{clean}\"*") }
}

/// How much of a message's text the preview keeps.
const PREVIEW_CHARS: usize = 1500;

const SUMMARY_COLUMNS: &str = "id, CASE WHEN from_name = '' THEN from_addr ELSE from_name END, from_addr, subject, preview, received, timestamp, unread, attach, message_id, in_reply_to, refs, pinned, starred, priority, labels";

/// The key as SQLCipher takes a raw one: `x'<hex>'`.
fn raw_key(key: &[u8; 32]) -> String {
    format!("x'{}'", key.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Opens the database file with `key` (or plain). Reading something proves the key is right.
fn open_file(path: &Path, key: Option<&[u8; 32]>) -> Result<Connection, MailError> {
    let attempt = |key: Option<&[u8; 32]>| -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open(path)?;
        if let Some(key) = key {
            conn.execute_batch(&format!("PRAGMA key = \"{}\";", raw_key(key)))?;
        }
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))?;
        Ok(conn)
    };
    if !path.exists() {
        return attempt(key).map_err(db);
    }
    match attempt(key) {
        Ok(conn) => Ok(conn),
        // A file written before the index was encrypted: encrypt it now.
        Err(_) if key.is_some() && attempt(None).is_ok() => {
            export_file(path, None, key)?;
            attempt(key).map_err(db)
        }
        Err(_) if key.is_none() => Err(MailError::Auth("the archive is encrypted".into())),
        Err(e) => Err(MailError::Auth(format!("the archive cannot be opened with this key: {e}"))),
    }
}

/// Rewrites the database file from `from` to `to` (a key, or plain) through SQLCipher's export, then
/// replaces the old file with the new one.
fn export_file(path: &Path, from: Option<&[u8; 32]>, to: Option<&[u8; 32]>) -> Result<(), MailError> {
    let tmp = path.with_extension("rewrite");
    let _ = std::fs::remove_file(&tmp);
    {
        let conn = Connection::open(path).map_err(db)?;
        if let Some(key) = from {
            conn.execute_batch(&format!("PRAGMA key = \"{}\";", raw_key(key))).map_err(db)?;
        }
        let _: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0)).map_err(db)?;
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        let target = to.map_or_else(String::new, raw_key);
        conn.execute("ATTACH DATABASE ?1 AS rewritten KEY ?2", params![tmp.to_string_lossy(), target]).map_err(db)?;
        conn.query_row("SELECT sqlcipher_export('rewritten')", [], |_| Ok(())).map_err(db)?;
        // The export copies tables and rows, not the schema version the migrations rely on.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(db)?;
        conn.execute_batch(&format!("PRAGMA rewritten.user_version = {version}; DETACH DATABASE rewritten;")).map_err(db)?;
    }
    remove_database(path);
    std::fs::rename(&tmp, path).map_err(db)
}

/// Deletes the database file and the files SQLite keeps next to it.
fn remove_database(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

/// Brings the tables up to the current version.
fn migrate(conn: &Connection) -> Result<(), MailError> {
        conn.execute_batch(SCHEMA).map_err(db)?;
        // Version 2: message bodies moved out of the database into encrypted files.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(db)?;
        if version < 2 {
            conn.execute_batch("DROP TABLE IF EXISTS bodies; PRAGMA user_version = 2;").map_err(db)?;
        }
        // Version 3: the size of each message and whether its file is stored, for the background download.
        if version < 3 {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN size INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE messages ADD COLUMN stored INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS messages_pending ON messages (account, folder, stored, timestamp DESC);
                 PRAGMA user_version = 3;",
            )
            .map_err(db)?;
        }
        // Version 4: how many messages the server said a folder holds.
        if version < 4 {
            conn.execute_batch("ALTER TABLE folders ADD COLUMN total INTEGER; PRAGMA user_version = 4;").map_err(db)?;
        }
        // Version 5: whether a message looks like it has attachments, for the paperclip in the list.
        if version < 5 {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN attach INTEGER NOT NULL DEFAULT 0; PRAGMA user_version = 5;").map_err(db)?;
        }
        // Version 6: whether `attach` is a real answer (from BODYSTRUCTURE or the message itself) or
        // only the default of a row written before it existed.
        if version < 6 {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN attach_known INTEGER NOT NULL DEFAULT 0; PRAGMA user_version = 6;").map_err(db)?;
        }
        // Version 7: the text of messages is searchable. `body_fts` keeps only the words, not the text
        // (contentless), and `indexed` says which messages are in it.
        if version < 7 {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN indexed INTEGER NOT NULL DEFAULT 0;
                 CREATE VIRTUAL TABLE IF NOT EXISTS body_fts USING fts5 (body, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2');
                 CREATE TRIGGER IF NOT EXISTS messages_body_ad AFTER DELETE ON messages BEGIN
                     DELETE FROM body_fts WHERE rowid = old.id;
                 END;
                 PRAGMA user_version = 7;",
            )
            .map_err(db)?;
        }
        // Version 8: the To and Cc addresses, to know whom the user writes to. NULL: not read yet.
        if version < 8 {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN recipients TEXT; PRAGMA user_version = 8;").map_err(db)?;
        }
        // Version 9: the fetched header fields, to check a message before its body is downloaded. NULL: not read yet.
        if version < 9 {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN headers BLOB; PRAGMA user_version = 9;").map_err(db)?;
        }
        // Version 10: the header fields kept include the spam filter's report; read them again.
        if version < 10 {
            conn.execute_batch("UPDATE messages SET headers = NULL; PRAGMA user_version = 10;").map_err(db)?;
        }
        // Version 11: the spam checks' verdict on each message, so only new mail is checked.
        if version < 11 {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN verdict TEXT; PRAGMA user_version = 11;").map_err(db)?;
        }
        // Version 12: messages the user called spam or not spam, kept as training examples after the
        // messages themselves move or the server empties its Spam folder.
        if version < 12 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS examples (
                     account TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     spam INTEGER NOT NULL,
                     text TEXT NOT NULL,
                     domain TEXT NOT NULL,
                     PRIMARY KEY (account, message_id)
                 );
                 PRAGMA user_version = 12;",
            )
            .map_err(db)?;
        }
        // Version 13: the preview keeps more of the text (the spam filter reads it), so stored messages
        // are read again to make it.
        if version < 13 {
            conn.execute_batch("UPDATE messages SET indexed = 0 WHERE stored = 1; PRAGMA user_version = 13;").map_err(db)?;
        }
        // Version 14: contacts, learned from the Sent folders as messages arrive. A learned one is a
        // suggestion for the address fields only, kept on this computer; `saved` marks the address book
        // the user keeps (and may sync), which nothing learned enters by itself.
        if version < 14 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS contacts (
                     address TEXT PRIMARY KEY,
                     name TEXT NOT NULL DEFAULT '',
                     folded TEXT NOT NULL,
                     saved INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE IF NOT EXISTS contact_accounts (
                     address TEXT NOT NULL,
                     account TEXT NOT NULL,
                     sent INTEGER NOT NULL,
                     last INTEGER NOT NULL,
                     PRIMARY KEY (address, account)
                 ) WITHOUT ROWID;
                 CREATE TABLE IF NOT EXISTS contacts_learned (
                     account TEXT NOT NULL,
                     message TEXT NOT NULL,
                     PRIMARY KEY (account, message)
                 ) WITHOUT ROWID;
                 CREATE INDEX IF NOT EXISTS messages_sender ON messages (lower(from_addr));",
            )
            .map_err(db)?;
            learn_archive(conn).map_err(db)?;
            conn.execute_batch("PRAGMA user_version = 14;").map_err(db)?;
        }
        // Version 15: notes on a contact.
        if version < 15 {
            conn.execute_batch("ALTER TABLE contacts ADD COLUMN notes TEXT NOT NULL DEFAULT ''; PRAGMA user_version = 15;").map_err(db)?;
        }
        // Version 16: the OpenPGP key each sender was first seen with, to check their signatures.
        if version < 16 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS peer_keys (address TEXT PRIMARY KEY, key BLOB NOT NULL, first_seen INTEGER NOT NULL);
                 PRAGMA user_version = 16;",
            )
            .map_err(db)?;
        }
        // Version 17: the S/MIME certificate each sender last signed with, to encrypt to them.
        if version < 17 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS peer_certs (address TEXT PRIMARY KEY, cert BLOB NOT NULL, seen INTEGER NOT NULL);
                 PRAGMA user_version = 17;",
            )
            .map_err(db)?;
        }
        // Version 18: the cards of address books synced over CardDAV, with what is not on the server yet.
        if version < 18 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cards (
                     id INTEGER PRIMARY KEY,
                     book TEXT NOT NULL,
                     href TEXT NOT NULL,
                     etag TEXT NOT NULL DEFAULT '',
                     vcard TEXT NOT NULL,
                     name TEXT NOT NULL,
                     emails TEXT NOT NULL,
                     phones TEXT NOT NULL,
                     note TEXT NOT NULL,
                     folded TEXT NOT NULL,
                     state INTEGER NOT NULL DEFAULT 0,
                     UNIQUE (book, href)
                 );
                 PRAGMA user_version = 18;",
            )
            .map_err(db)?;
        }
        // Version 22: calendars' events, with when they happen for the views' ranges.
        if version < 22 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS events (
                     id INTEGER PRIMARY KEY,
                     calendar TEXT NOT NULL,
                     href TEXT NOT NULL,
                     etag TEXT NOT NULL DEFAULT '',
                     ics TEXT NOT NULL,
                     uid TEXT NOT NULL DEFAULT '',
                     first INTEGER NOT NULL DEFAULT 0,
                     last INTEGER,
                     state INTEGER NOT NULL DEFAULT 0,
                     UNIQUE (calendar, href)
                 );
                 CREATE INDEX IF NOT EXISTS events_uid ON events (uid);
                 PRAGMA user_version = 22;",
            )
            .map_err(db)?;
        }
        if version < 19 {
            // Run again from an older version (as a test does), the column is already there.
            let have = conn.prepare("SELECT pinned FROM messages LIMIT 0").is_ok();
            let add = if have { "" } else { "ALTER TABLE messages ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;" };
            conn.execute_batch(&format!("{add} PRAGMA user_version = 19;")).map_err(db)?;
        }
        if version < 20 {
            // The pins of version 19 were the server's \Flagged: kept as stars, and nothing is pinned yet.
            let have = conn.prepare("SELECT starred FROM messages LIMIT 0").is_ok();
            let add = if have { "" } else { "ALTER TABLE messages ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;" };
            conn.execute_batch(&format!("{add} UPDATE messages SET starred = pinned, pinned = 0; PRAGMA user_version = 20;")).map_err(db)?;
        }
        if version < 21 {
            let mut add = String::new();
            for (column, kind) in [("priority", "INTEGER NOT NULL DEFAULT 0"), ("labels", "TEXT NOT NULL DEFAULT ''")] {
                if conn.prepare(&format!("SELECT {column} FROM messages LIMIT 0")).is_err() {
                    add += &format!("ALTER TABLE messages ADD COLUMN {column} {kind};");
                }
            }
            conn.execute_batch(&format!("{add} PRAGMA user_version = 21;")).map_err(db)?;
        }
        // Version 23: the server's separator between a folder and those inside it, as LIST gives it.
        if version < 23 {
            let have = conn.prepare("SELECT delimiter FROM folders LIMIT 0").is_ok();
            let add = if have { "" } else { "ALTER TABLE folders ADD COLUMN delimiter TEXT;" };
            conn.execute_batch(&format!("{add} PRAGMA user_version = 23;")).map_err(db)?;
        }
    Ok(())
}

/// Learns the contacts of everything already in the archive: whom each Sent message went to.
fn learn_archive(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT m.account, m.folder, m.uid, m.message_id, m.recipients, m.timestamp FROM messages m JOIN folders f ON f.account = m.account AND f.name = m.folder
         WHERE f.role = ?1 AND m.recipients IS NOT NULL",
    )?;
    let sent = stmt
        .query_map([role_to_int(Role::Sent)], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u32, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (account, folder, uid, message_id, recipients, at) in sent {
        learn_sent(conn, &account, &learn_key(&message_id, &folder, uid), &recipients, at)?;
    }
    Ok(())
}

impl Store {
    /// Opens (and creates) the archive at `path`, encrypted with `key` (SQLCipher) or plain when `key`
    /// is `None`. A plain file opened with a key is encrypted in place; an encrypted file opened without
    /// one is [`MailError::Auth`], because it cannot be read.
    pub fn open(path: &Path, key: Option<&[u8; 32]>) -> Result<Self, MailError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(db)?;
        }
        let conn = open_file(path, key)?;
        Self::with(conn, Some(path.to_path_buf()), key.copied())
    }

    /// An empty stand-in for the archive at `path`, for as long as its key is not known. Call
    /// [`Store::unlock`] when it is.
    pub fn locked(path: &Path) -> Result<Self, MailError> {
        let store = Self::with(Connection::open_in_memory().map_err(db)?, Some(path.to_path_buf()), None)?;
        store.conn().locked = true;
        Ok(store)
    }

    /// A throwaway archive in memory, for tests.
    pub fn in_memory() -> Result<Self, MailError> {
        Self::with(Connection::open_in_memory().map_err(db)?, None, None)
    }

    fn with(conn: Connection, path: Option<PathBuf>, key: Option<[u8; 32]>) -> Result<Self, MailError> {
        migrate(&conn)?;
        Ok(Self { inner: Mutex::new(Inner { conn, path, key, locked: false }) })
    }

    /// Whether this is still the empty stand-in made by [`Store::locked`].
    pub fn is_locked(&self) -> bool {
        self.conn().locked
    }

    /// Opens the real archive of a [`Store::locked`] one, with its key.
    pub fn unlock(&self, key: Option<&[u8; 32]>) -> Result<(), MailError> {
        let mut inner = self.conn();
        let Some(path) = inner.path.clone() else { return Ok(()) };
        let conn = open_file(&path, key)?;
        migrate(&conn)?;
        *inner = Inner { conn, path: Some(path), key: key.copied(), locked: false };
        Ok(())
    }

    /// Changes how the file is kept: plain to encrypted, encrypted to plain, or under another key. A
    /// no-op for a database in memory and when nothing changes.
    pub fn rekey(&self, key: Option<&[u8; 32]>) -> Result<(), MailError> {
        let mut inner = self.conn();
        let Some(path) = inner.path.clone() else { return Ok(()) };
        if inner.key.as_ref() == key {
            return Ok(());
        }
        match (inner.key, key) {
            (Some(_), Some(new)) => {
                inner.conn.execute_batch(&format!("PRAGMA rekey = \"{}\";", raw_key(new))).map_err(db)?;
            }
            (current, new) => {
                // Close the file, rewrite it through an export, and open the result.
                inner.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").map_err(db)?;
                inner.conn = Connection::open_in_memory().map_err(db)?;
                export_file(&path, current.as_ref(), new)?;
                inner.conn = open_file(&path, new)?;
            }
        }
        inner.key = key.copied();
        Ok(())
    }

    /// Throws the archive away and starts an empty one (the key was lost: the mail is downloaded again).
    pub fn reset(&self, key: Option<&[u8; 32]>) -> Result<(), MailError> {
        let mut inner = self.conn();
        let Some(path) = inner.path.clone() else { return Ok(()) };
        inner.conn = Connection::open_in_memory().map_err(db)?;
        remove_database(&path);
        let conn = open_file(&path, key)?;
        migrate(&conn)?;
        *inner = Inner { conn, path: Some(path), key: key.copied(), locked: false };
        Ok(())
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while the lock was held leaves consistent data (every write is a transaction).
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the folders the server lists and forgets those that no longer exist.
    pub fn set_folders(&self, account: &str, folders: &[(String, Role)]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        for (name, role) in folders {
            tx.execute(
                "INSERT INTO folders (account, name, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT (account, name) DO UPDATE SET role = excluded.role",
                params![account, name, role_to_int(*role)],
            )
            .map_err(db)?;
        }
        let known: Vec<String> = tx
            .prepare("SELECT name FROM folders WHERE account = ?1")
            .map_err(db)?
            .query_map([account], |r| r.get(0))
            .map_err(db)?
            .collect::<Result<_, _>>()
            .map_err(db)?;
        for name in known.iter().filter(|n| !folders.iter().any(|(f, _)| f == *n)) {
            tx.execute("DELETE FROM messages WHERE account = ?1 AND folder = ?2", params![account, name]).map_err(db)?;
            tx.execute("DELETE FROM folders WHERE account = ?1 AND name = ?2", params![account, name]).map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    /// Deletes everything the archive holds for an account (it was removed or its server changed).
    pub fn forget_account(&self, account: &str) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM messages WHERE account = ?1", [account]).map_err(db)?;
        tx.execute("DELETE FROM folders WHERE account = ?1", [account]).map_err(db)?;
        tx.execute("DELETE FROM examples WHERE account = ?1", [account]).map_err(db)?;
        tx.execute("DELETE FROM contact_accounts WHERE account = ?1", [account]).map_err(db)?;
        tx.execute("DELETE FROM contacts_learned WHERE account = ?1", [account]).map_err(db)?;
        // Suggestions only this account had written to go; saved contacts stay.
        tx.execute("DELETE FROM contacts WHERE saved = 0 AND address NOT IN (SELECT address FROM contact_accounts)", []).map_err(db)?;
        tx.commit().map_err(db)
    }

    /// The account's folders with unread counts from the archive, in no particular order.
    pub fn folders(&self, account: &str) -> Result<Vec<FolderRow>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT f.name, f.role,
                        (SELECT COUNT(*) FROM messages m WHERE m.account = f.account AND m.folder = f.name AND m.unread = 1),
                        COALESCE(f.total, (SELECT COUNT(*) FROM messages m WHERE m.account = f.account AND m.folder = f.name)),
                        (SELECT COUNT(*) FROM messages m WHERE m.account = f.account AND m.folder = f.name AND m.stored = 1),
                        f.delimiter
                 FROM folders f WHERE f.account = ?1",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([account], |r| {
                let delimiter = r.get::<_, Option<String>>(5)?.and_then(|d| d.chars().next());
                Ok(FolderRow { name: r.get(0)?, role: role_from_int(r.get(1)?), unread: r.get::<_, i64>(2)? as u32, total: r.get::<_, i64>(3)? as u32, stored: r.get::<_, i64>(4)? as u32, delimiter })
            })
            .map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Remembers how many messages the server says a folder holds.
    pub fn set_total(&self, account: &str, folder: &str, total: u32) -> Result<(), MailError> {
        self.conn().execute("UPDATE folders SET total = ?3 WHERE account = ?1 AND name = ?2", params![account, folder, i64::from(total)]).map(|_| ()).map_err(db)
    }

    /// Remembers the separator the server gave with a folder in its list.
    pub fn set_delimiter(&self, account: &str, folder: &str, delimiter: Option<char>) -> Result<(), MailError> {
        self.conn().execute("UPDATE folders SET delimiter = ?3 WHERE account = ?1 AND name = ?2", params![account, folder, delimiter.map(String::from)]).map(|_| ()).map_err(db)
    }

    /// Renames folder `from` to `to`, with the folders inside it (`from` + `delimiter` + …), as the server
    /// just did; their messages keep their ids, flags and what is kept only here. Returns each (old, new)
    /// name, for their files to follow.
    pub fn rename_folder(&self, account: &str, from: &str, to: &str, delimiter: char) -> Result<Vec<(String, String)>, MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        let names: Vec<String> = tx
            .prepare("SELECT name FROM folders WHERE account = ?1")
            .map_err(db)?
            .query_map([account], |r| r.get(0))
            .map_err(db)?
            .collect::<Result<_, _>>()
            .map_err(db)?;
        let inside = format!("{from}{delimiter}");
        let moved: Vec<(String, String)> = names
            .into_iter()
            .filter_map(|name| match name.strip_prefix(&inside) {
                _ if name == from => Some((name, to.to_owned())),
                Some(rest) => Some((name.clone(), format!("{to}{delimiter}{rest}"))),
                None => None,
            })
            .collect();
        for (old, new) in &moved {
            tx.execute("UPDATE folders SET name = ?3 WHERE account = ?1 AND name = ?2", params![account, old, new]).map_err(db)?;
            tx.execute("UPDATE messages SET folder = ?3 WHERE account = ?1 AND folder = ?2", params![account, old, new]).map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(moved)
    }

    /// Forgets a folder the server deleted, with its messages.
    pub fn forget_folder(&self, account: &str, folder: &str) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM messages WHERE account = ?1 AND folder = ?2", params![account, folder]).map_err(db)?;
        tx.execute("DELETE FROM folders WHERE account = ?1 AND name = ?2", params![account, folder]).map_err(db)?;
        tx.commit().map_err(db)
    }

    pub fn folder_state(&self, account: &str, folder: &str) -> Result<FolderState, MailError> {
        let conn = self.conn();
        let state = conn
            .query_row("SELECT uidvalidity, floor_uid FROM folders WHERE account = ?1 AND name = ?2", params![account, folder], |r| {
                Ok(FolderState { uidvalidity: r.get::<_, Option<i64>>(0)?.map(|v| v as u32), floor_uid: r.get::<_, Option<i64>>(1)?.map(|v| v as u32) })
            })
            .optional()
            .map_err(db)?;
        Ok(state.unwrap_or(FolderState { uidvalidity: None, floor_uid: None }))
    }

    /// The server's UIDVALIDITY changed (or this is the first sync): every cached UID is meaningless.
    pub fn reset_folder(&self, account: &str, folder: &str, uidvalidity: u32) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM messages WHERE account = ?1 AND folder = ?2", params![account, folder]).map_err(db)?;
        tx.execute(
            "UPDATE folders SET uidvalidity = ?3, floor_uid = NULL WHERE account = ?1 AND name = ?2",
            params![account, folder, i64::from(uidvalidity)],
        )
        .map_err(db)?;
        tx.commit().map_err(db)
    }

    /// Remembers that every message with a UID at or above `floor` has been downloaded.
    pub fn set_floor(&self, account: &str, folder: &str, floor: u32) -> Result<(), MailError> {
        self.conn()
            .execute("UPDATE folders SET floor_uid = ?3 WHERE account = ?1 AND name = ?2", params![account, folder, i64::from(floor)])
            .map(|_| ())
            .map_err(db)
    }

    /// Adds headers; a UID already in the archive keeps its row (and its body and preview). Sent
    /// messages teach whom the user writes to; other mail, outside Spam, how they name themselves.
    pub fn add_headers(&self, account: &str, folder: &str, headers: &[Header]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        let role = tx.query_row("SELECT role FROM folders WHERE account = ?1 AND name = ?2", params![account, folder], |r| r.get(0)).optional().map_err(db)?.map(role_from_int);
        for h in headers {
            match role {
                Some(Role::Sent) => learn_sent(&tx, account, &learn_key(&h.message_id, folder, h.uid), &h.recipients, h.timestamp).map_err(db)?,
                Some(Role::Junk) => {}
                _ => learn_name(&tx, &h.from_addr, &h.from_name).map_err(db)?,
            }
        }
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR IGNORE INTO messages
                     (account, folder, uid, from_name, from_addr, subject, received, timestamp, unread, message_id, in_reply_to, refs, size, attach, attach_known, recipients, headers, pinned, starred, priority, labels)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1, ?15, ?16, ?17, ?18, ?19, ?20)",
                )
                .map_err(db)?;
            for h in headers {
                stmt.execute(params![
                    account, folder, i64::from(h.uid), h.from_name, h.from_addr, h.subject, h.received, h.timestamp, i64::from(h.unread),
                    h.message_id, h.in_reply_to, h.references, i64::from(h.size), i64::from(h.has_attachments), h.recipients, h.raw, i64::from(h.pinned), i64::from(h.starred), i64::from(h.priority), h.labels
                ])
                .map_err(db)?;
            }
        }
        tx.commit().map_err(db)
    }

    /// Newest first.
    pub fn messages(&self, account: &str, folder: &str) -> Result<Vec<MessageSummary>, MailError> {
        let conn = self.conn();
        let sql = format!("SELECT {SUMMARY_COLUMNS} FROM messages WHERE account = ?1 AND folder = ?2 ORDER BY timestamp DESC, uid DESC");
        let mut stmt = conn.prepare(&sql).map_err(db)?;
        let rows = stmt.query_map(params![account, folder], summary).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// (lowest UID, highest UID, count) of the folder's cached messages.
    pub fn uid_range(&self, account: &str, folder: &str) -> Result<Option<(u32, u32, u32)>, MailError> {
        let conn = self.conn();
        conn.query_row("SELECT MIN(uid), MAX(uid), COUNT(*) FROM messages WHERE account = ?1 AND folder = ?2", params![account, folder], |r| {
            Ok(r.get::<_, Option<i64>>(0)?.zip(r.get::<_, Option<i64>>(1)?).map(|(lo, hi)| (lo as u32, hi as u32, r.get::<_, i64>(2).unwrap_or(0) as u32)))
        })
        .map_err(db)
    }

    /// The cached UIDs of a folder at or above `from`, with their unread flag.
    /// The cached messages of a folder from UID `from` up, as (uid, [`Flags`]).
    pub fn flags_since(&self, account: &str, folder: &str, from: u32) -> Result<Vec<(u32, Flags)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT uid, unread, pinned, starred, labels FROM messages WHERE account = ?1 AND folder = ?2 AND uid >= ?3").map_err(db)?;
        let rows = stmt
            .query_map(params![account, folder, i64::from(from)], |r| {
                Ok((r.get::<_, i64>(0)? as u32, Flags { unread: r.get::<_, i64>(1)? != 0, pinned: r.get::<_, i64>(2)? != 0, starred: r.get::<_, i64>(3)? != 0, labels: r.get(4)? }))
            })
            .map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Sets the flags of messages as the server has them, by UID.
    pub fn set_flags_by_uid(&self, account: &str, folder: &str, uids: &[(u32, Flags)]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        for (uid, f) in uids {
            tx.execute(
                "UPDATE messages SET unread = ?4, pinned = ?5, starred = ?6, labels = ?7 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, i64::from(*uid), i64::from(f.unread), i64::from(f.pinned), i64::from(f.starred), f.labels],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    pub fn remove_uids(&self, account: &str, folder: &str, uids: &[u32]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        for uid in uids {
            tx.execute("DELETE FROM messages WHERE account = ?1 AND folder = ?2 AND uid = ?3", params![account, folder, i64::from(*uid)]).map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    /// UIDs of a folder whose attachment flag was never set from the server's answer.
    pub fn unknown_attachments(&self, account: &str, folder: &str, limit: usize) -> Result<Vec<u32>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT uid FROM messages WHERE account = ?1 AND folder = ?2 AND attach_known = 0 ORDER BY timestamp DESC LIMIT ?3")
            .map_err(db)?;
        let rows = stmt.query_map(params![account, folder, limit as i64], |r| r.get::<_, i64>(0).map(|u| u as u32)).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Records whether a message carries attachments, as the server or the message itself says.
    pub fn set_attachments(&self, account: &str, folder: &str, uid: u32, has: bool) -> Result<(), MailError> {
        self.conn()
            .execute(
                "UPDATE messages SET attach = ?4, attach_known = 1 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, i64::from(uid), i64::from(has)],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// UIDs of a folder whose recipients were never read (rows from before they were kept).
    pub fn unknown_recipients(&self, account: &str, folder: &str, limit: usize) -> Result<Vec<u32>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT uid FROM messages WHERE account = ?1 AND folder = ?2 AND recipients IS NULL ORDER BY timestamp DESC LIMIT ?3").map_err(db)?;
        let rows = stmt.query_map(params![account, folder, limit as i64], |r| r.get::<_, i64>(0).map(|u| u as u32)).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Records the To and Cc addresses of a message (lower case, separated by spaces); in Sent, its
    /// recipients are learned as contacts then.
    pub fn set_recipients(&self, account: &str, folder: &str, uid: u32, recipients: &str) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("UPDATE messages SET recipients = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3", params![account, folder, i64::from(uid), recipients]).map_err(db)?;
        let sent = tx
            .query_row(
                "SELECT m.message_id, m.timestamp FROM messages m JOIN folders f ON f.account = m.account AND f.name = m.folder WHERE m.account = ?1 AND m.folder = ?2 AND m.uid = ?3 AND f.role = ?4",
                params![account, folder, i64::from(uid), role_to_int(Role::Sent)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(db)?;
        if let Some((message_id, at)) = sent {
            learn_sent(&tx, account, &learn_key(&message_id, folder, uid), recipients, at).map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    /// UIDs of a folder whose header fields were never kept (rows from before they were).
    pub fn unknown_headers(&self, account: &str, folder: &str, limit: usize) -> Result<Vec<u32>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT uid FROM messages WHERE account = ?1 AND folder = ?2 AND headers IS NULL ORDER BY timestamp DESC LIMIT ?3").map_err(db)?;
        let rows = stmt.query_map(params![account, folder, limit as i64], |r| r.get::<_, i64>(0).map(|u| u as u32)).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    pub fn set_headers(&self, account: &str, folder: &str, uid: u32, raw: &[u8]) -> Result<(), MailError> {
        self.conn()
            .execute("UPDATE messages SET headers = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3", params![account, folder, i64::from(uid), raw])
            .map(|_| ())
            .map_err(db)
    }

    /// The sender and kept header fields of an account's latest messages, newest first.
    pub fn recent_headers(&self, account: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT from_addr, headers FROM messages WHERE account = ?1 AND length(headers) > 0 ORDER BY timestamp DESC LIMIT ?2").map_err(db)?;
        let rows = stmt.query_map(params![account, limit as i64], |r| Ok((r.get(0)?, r.get(1)?))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// The header fields kept for a message; `None` when they were not read yet.
    pub fn headers(&self, id: MessageId) -> Result<Option<Vec<u8>>, MailError> {
        self.conn().query_row("SELECT headers FROM messages WHERE id = ?1", [id.0 as i64], |r| r.get(0)).optional().map(Option::flatten).map_err(db)
    }

    /// The spam checks' verdicts kept for the messages of a folder. The archive does not read them.
    pub fn verdicts(&self, account: &str, folder: &str) -> Result<Vec<(MessageId, String)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, verdict FROM messages WHERE account = ?1 AND folder = ?2 AND verdict IS NOT NULL").map_err(db)?;
        let rows = stmt.query_map(params![account, folder], |r| Ok((MessageId(r.get::<_, i64>(0)? as u64), r.get(1)?))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    pub fn set_verdicts(&self, verdicts: &[(MessageId, String)]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        {
            let mut stmt = tx.prepare("UPDATE messages SET verdict = ?2 WHERE id = ?1").map_err(db)?;
            for (id, verdict) in verdicts {
                stmt.execute(params![id.0 as i64, verdict]).map_err(db)?;
            }
        }
        tx.commit().map_err(db)
    }

    /// Forgets a folder's verdicts, so every message is checked again.
    pub fn clear_verdicts(&self, account: &str, folder: &str) -> Result<(), MailError> {
        self.conn().execute("UPDATE messages SET verdict = NULL WHERE account = ?1 AND folder = ?2", params![account, folder]).map(|_| ()).map_err(db)
    }

    /// Keeps a message the user called spam (or not spam) as a training example; a later call on the
    /// same message replaces it, so changing one's mind does not count twice.
    pub fn add_example(&self, account: &str, message_id: &str, spam: bool, text: &str, domain: &str) -> Result<(), MailError> {
        self.conn()
            .execute("INSERT OR REPLACE INTO examples (account, message_id, spam, text, domain) VALUES (?1, ?2, ?3, ?4, ?5)", params![account, message_id, i64::from(spam), text, domain])
            .map(|_| ())
            .map_err(db)
    }

    /// Forgets every message the user called spam or not spam, on all accounts.
    pub fn clear_examples(&self) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM examples", []).map(|_| ()).map_err(db)
    }

    /// The training examples of an account: (message id, spam, text, sender domain).
    pub fn examples(&self, account: &str) -> Result<Vec<(String, bool, String, String)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT message_id, spam, text, domain FROM examples WHERE account = ?1").map_err(db)?;
        let rows = stmt.query_map([account], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?, r.get(3)?))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Everyone the user has written to, from any of their accounts: the recipients of every Sent
    /// folder, lower case. Someone written to from work is not a stranger at home.
    pub fn correspondents(&self) -> Result<std::collections::HashSet<String>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT m.recipients FROM messages m JOIN folders f ON f.account = m.account AND f.name = m.folder WHERE f.role = ?1 AND m.recipients IS NOT NULL")
            .map_err(db)?;
        let rows = stmt.query_map(params![role_to_int(Role::Sent)], |r| r.get::<_, String>(0)).map_err(db)?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.extend(row.map_err(db)?.split_whitespace().map(str::to_owned));
        }
        Ok(out)
    }

    /// The contacts to suggest while writing from `account`: the saved ones and everyone written to
    /// from any account, those this account writes to most first, then those written to most, then the
    /// latest. Senders never written to are not here: one may be a stranger or a spammer.
    pub fn contacts(&self, account: &str) -> Result<Vec<Contact>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT c.name, c.address, c.folded FROM contacts c
                 LEFT JOIN (SELECT address, SUM(sent) AS sent, MAX(last) AS last, SUM(CASE WHEN account = ?1 THEN sent ELSE 0 END) AS mine
                            FROM contact_accounts GROUP BY address) a ON a.address = c.address
                 WHERE a.address IS NOT NULL OR c.saved = 1
                 ORDER BY COALESCE(a.mine, 0) DESC, COALESCE(a.sent, 0) DESC, COALESCE(a.last, 0) DESC, c.address",
            )
            .map_err(db)?;
        let rows = stmt.query_map([account], |r| Ok(Contact { name: r.get(0)?, address: r.get(1)?, folded: r.get(2)? })).map_err(db)?;
        let mut contacts: Vec<Contact> = rows.collect::<Result<_, _>>().map_err(db)?;
        drop(stmt);
        drop(conn);
        // The address books' people too, after those written to, by the name the address book gives.
        let cards = self.address_book()?;
        // A name the address book keeps wins over the one mail gave.
        for card in cards.iter().filter(|c| !c.name.is_empty()) {
            for contact in contacts.iter_mut().filter(|c| card.emails.contains(&c.address)) {
                *contact = Contact::new(&card.name, &contact.address);
            }
        }
        let mut seen: std::collections::HashSet<String> = contacts.iter().map(|c| c.address.clone()).collect();
        for card in &cards {
            for email in card.emails.iter().filter(|e| seen.insert((*e).clone())) {
                contacts.push(Contact::new(&card.name, email));
            }
        }
        Ok(contacts)
    }

    /// The OpenPGP key `address` was first seen with, to check what they sign.
    pub fn peer_key(&self, address: &str) -> Result<Option<Vec<u8>>, MailError> {
        self.conn().query_row("SELECT key FROM peer_keys WHERE address = ?1", [address.trim().to_lowercase()], |r| r.get(0)).optional().map_err(db)
    }

    /// Keeps `key` for `address` unless one is kept already: the first key seen is the one trusted, so
    /// a message bringing another cannot replace it.
    pub fn learn_peer_key(&self, address: &str, key: &[u8], at: i64) -> Result<(), MailError> {
        self.conn()
            .execute("INSERT OR IGNORE INTO peer_keys (address, key, first_seen) VALUES (?1, ?2, ?3)", params![address.trim().to_lowercase(), key, at])
            .map(|_| ())
            .map_err(db)
    }

    /// The S/MIME certificate `address` last signed with.
    pub fn peer_cert(&self, address: &str) -> Result<Option<Vec<u8>>, MailError> {
        self.conn().query_row("SELECT cert FROM peer_certs WHERE address = ?1", [address.trim().to_lowercase()], |r| r.get(0)).optional().map_err(db)
    }

    /// Keeps the certificate a valid signature from `address` was made with. Unlike an OpenPGP key it
    /// replaces the one before: an authority vouched for it, and certificates are renewed.
    pub fn learn_peer_cert(&self, address: &str, cert: &[u8], at: i64) -> Result<(), MailError> {
        self.conn()
            .execute("INSERT OR REPLACE INTO peer_certs (address, cert, seen) VALUES (?1, ?2, ?3)", params![address.trim().to_lowercase(), cert, at])
            .map(|_| ())
            .map_err(db)
    }

    /// Every card of the address book at `book`, deleted ones included (they still have to go).
    pub fn cards(&self, book: &str) -> Result<Vec<StoredCard>, MailError> {
        self.cards_where("WHERE book = ?1", [book])
    }

    /// The cards of every address book, as the Contacts section lists them: not deleted, by name.
    pub fn address_book(&self) -> Result<Vec<StoredCard>, MailError> {
        self.cards_where("WHERE state != 3 ORDER BY lower(name), id", [])
    }

    /// The address-book card that has `address` among its addresses, if one does.
    pub fn card_with_email(&self, address: &str) -> Result<Option<StoredCard>, MailError> {
        Ok(self.cards_where("WHERE state != 3 AND instr(char(10) || emails || char(10), char(10) || ?1 || char(10)) > 0 ORDER BY id", [address.trim().to_lowercase()])?.into_iter().next())
    }

    pub fn card(&self, id: i64) -> Result<Option<StoredCard>, MailError> {
        Ok(self.cards_where("WHERE id = ?1", [id])?.pop())
    }

    fn cards_where<P: rusqlite::Params>(&self, filter: &str, params: P) -> Result<Vec<StoredCard>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT id, book, href, etag, vcard, name, emails, phones, note, state FROM cards {filter}")).map_err(db)?;
        let split = |text: String| text.split('\n').filter(|s| !s.is_empty()).map(str::to_owned).collect::<Vec<_>>();
        let rows = stmt
            .query_map(params, |r| {
                Ok(StoredCard {
                    id: r.get(0)?,
                    book: r.get(1)?,
                    href: r.get(2)?,
                    etag: r.get(3)?,
                    vcard: r.get(4)?,
                    name: r.get(5)?,
                    emails: split(r.get(6)?),
                    phones: split(r.get(7)?),
                    note: r.get(8)?,
                    state: CardState::from_int(r.get(9)?),
                })
            })
            .map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Keeps a card as the server has it (new, or a newer version), as synced.
    pub fn put_card(&self, book: &str, href: &str, etag: &str, vcard: &str, fields: &CardFields) -> Result<(), MailError> {
        self.write_card(book, href, etag, vcard, fields, CardState::Synced)
    }

    /// A card made here, to send: `href` is where it will go.
    pub fn add_card(&self, book: &str, href: &str, vcard: &str, fields: &CardFields) -> Result<(), MailError> {
        self.write_card(book, href, "", vcard, fields, CardState::New)
    }

    fn write_card(&self, book: &str, href: &str, etag: &str, vcard: &str, fields: &CardFields, state: CardState) -> Result<(), MailError> {
        self.conn()
            .execute(
                "INSERT INTO cards (book, href, etag, vcard, name, emails, phones, note, folded, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (book, href) DO UPDATE SET etag = excluded.etag, vcard = excluded.vcard, name = excluded.name, emails = excluded.emails,
                   phones = excluded.phones, note = excluded.note, folded = excluded.folded, state = excluded.state",
                params![book, href, etag, vcard, fields.name, fields.emails.join("\n"), fields.phones.join("\n"), fields.note, fold(&format!("{} {} {}", fields.name, fields.emails.join(" "), fields.phones.join(" "))), state.to_int()],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// A card changed here: its new text and what it shows, to send. One not sent yet stays new.
    pub fn edit_card(&self, id: i64, vcard: &str, fields: &CardFields) -> Result<(), MailError> {
        self.conn()
            .execute(
                "UPDATE cards SET vcard = ?2, name = ?3, emails = ?4, phones = ?5, note = ?6, folded = ?7, state = CASE WHEN state = 2 THEN 2 ELSE 1 END WHERE id = ?1",
                params![id, vcard, fields.name, fields.emails.join("\n"), fields.phones.join("\n"), fields.note, fold(&format!("{} {} {}", fields.name, fields.emails.join(" "), fields.phones.join(" ")))],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// A card deleted here: one never sent goes at once, any other waits to be deleted on the server.
    pub fn delete_card(&self, id: i64) -> Result<(), MailError> {
        let conn = self.conn();
        conn.execute("DELETE FROM cards WHERE id = ?1 AND state = 2", [id]).map_err(db)?;
        conn.execute("UPDATE cards SET state = 3 WHERE id = ?1", [id]).map(|_| ()).map_err(db)
    }

    /// The card is as the server has it now, at this version.
    pub fn card_synced(&self, id: i64, etag: &str) -> Result<(), MailError> {
        self.conn().execute("UPDATE cards SET etag = ?2, state = 0 WHERE id = ?1", params![id, etag]).map(|_| ()).map_err(db)
    }

    /// Forgets a card: gone from the server, or its deletion done.
    pub fn forget_card(&self, book: &str, href: &str) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM cards WHERE book = ?1 AND href = ?2", params![book, href]).map(|_| ()).map_err(db)
    }

    /// The events of a calendar, as stored, for its sync.
    pub fn events(&self, calendar: &str) -> Result<Vec<StoredEvent>, MailError> {
        self.query_events("WHERE calendar = ?1", params![calendar])
    }

    /// The events of `calendars` that may have an occurrence between `from` and `to` (seconds since
    /// the epoch): those whose span overlaps it. Their occurrences are worked out by the caller.
    pub fn events_between(&self, calendars: &[String], from: i64, to: i64) -> Result<Vec<StoredEvent>, MailError> {
        let list = calendars.iter().map(|c| format!("'{}'", c.replace('\'', "''"))).collect::<Vec<_>>().join(", ");
        self.query_events(&format!("WHERE calendar IN ({list}) AND state != 3 AND first < ?1 AND (last IS NULL OR last > ?2)"), params![to, from])
    }

    /// The stored copies of the event with this `UID`, in every calendar.
    pub fn events_with_uid(&self, uid: &str) -> Result<Vec<StoredEvent>, MailError> {
        self.query_events("WHERE uid = ?1 AND state != 3", params![uid])
    }

    fn query_events(&self, filter: &str, args: impl rusqlite::Params) -> Result<Vec<StoredEvent>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT id, calendar, href, etag, ics, state FROM events {filter}")).map_err(db)?;
        let rows = stmt
            .query_map(args, |r| Ok(StoredEvent { id: r.get(0)?, calendar: r.get(1)?, href: r.get(2)?, etag: r.get(3)?, ics: r.get(4)?, state: CardState::from_int(r.get(5)?) }))
            .map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Keeps an event as the server has it (or, for the calendar on this computer, as made here).
    pub fn put_event(&self, calendar: &str, href: &str, etag: &str, ics: &str, span: &EventSpan) -> Result<(), MailError> {
        self.conn()
            .execute(
                "INSERT INTO events (calendar, href, etag, ics, uid, first, last, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
                 ON CONFLICT (calendar, href) DO UPDATE SET etag = ?3, ics = ?4, uid = ?5, first = ?6, last = ?7, state = 0",
                params![calendar, href, etag, ics, span.uid, span.first, span.last],
            )
            .map(|_| ())
            .map_err(db)
    }

    pub fn forget_event(&self, calendar: &str, href: &str) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM events WHERE calendar = ?1 AND href = ?2", params![calendar, href]).map(|_| ()).map_err(db)
    }

    /// Forgets a calendar's events: it was removed here.
    pub fn forget_calendar(&self, calendar: &str) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM events WHERE calendar = ?1", [calendar]).map(|_| ()).map_err(db)
    }

    /// Forgets an address book's cards: its sync was turned off.
    pub fn forget_book(&self, book: &str) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM cards WHERE book = ?1", [book]).map(|_| ()).map_err(db)
    }

    /// Someone's card: as the address book or mail knows them; `None` for someone never written to nor saved.
    pub fn contact_card(&self, address: &str) -> Result<Option<ContactCard>, MailError> {
        self.conn()
            .query_row(
                "SELECT c.name, c.address, c.saved, c.notes, COALESCE(SUM(a.sent), 0), MAX(a.last) FROM contacts c LEFT JOIN contact_accounts a ON a.address = c.address WHERE c.address = ?1 GROUP BY c.address",
                [address.trim().to_lowercase()],
                |r| Ok(ContactCard { name: r.get(0)?, address: r.get(1)?, saved: r.get::<_, i64>(2)? != 0, notes: r.get(3)?, sent: r.get::<_, i64>(4)? as u32, last_sent: r.get(5)? }),
            )
            .optional()
            .map_err(db)
    }

    /// Puts someone in the address book with this name and notes; mail never renames them after.
    pub fn save_contact(&self, address: &str, name: &str, notes: &str) -> Result<(), MailError> {
        let address = address.trim().to_lowercase();
        self.conn()
            .execute(
                "INSERT INTO contacts (address, name, folded, saved, notes) VALUES (?1, ?2, ?3, 1, ?4)
                 ON CONFLICT (address) DO UPDATE SET name = excluded.name, folded = excluded.folded, saved = 1, notes = excluded.notes",
                params![address, name.trim(), fold(&format!("{} {address}", name.trim())), notes.trim_end()],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// Takes someone out of the address book, notes and all: they stay a suggestion if the user wrote to them.
    pub fn unsave_contact(&self, address: &str) -> Result<(), MailError> {
        let address = address.trim().to_lowercase();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("UPDATE contacts SET saved = 0, notes = '' WHERE address = ?1", [&address]).map_err(db)?;
        tx.execute("DELETE FROM contacts WHERE address = ?1 AND address NOT IN (SELECT address FROM contact_accounts)", [&address]).map_err(db)?;
        tx.commit().map_err(db)
    }

    /// The address book: the contacts the user saved, by name.
    pub fn saved_contacts(&self) -> Result<Vec<Contact>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, address, folded FROM contacts WHERE saved = 1 ORDER BY CASE WHEN name = '' THEN address ELSE lower(name) END").map_err(db)?;
        let rows = stmt.query_map([], |r| Ok(Contact { name: r.get(0)?, address: r.get(1)?, folded: r.get(2)? })).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// The latest messages from someone, or sent to them, in every account, as (account, where it
    /// is); Spam and Trash are left out.
    pub fn messages_with(&self, address: &str, limit: usize) -> Result<Vec<(String, Hit)>, MailError> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS}, folder, account FROM messages m
             WHERE (lower(from_addr) = ?1 OR instr(' ' || COALESCE(recipients, '') || ' ', ' ' || ?1 || ' ') > 0)
               AND folder NOT IN (SELECT name FROM folders f WHERE f.account = m.account AND f.role IN (?2, ?3))
             ORDER BY timestamp DESC LIMIT ?4"
        );
        let mut stmt = conn.prepare(&sql).map_err(db)?;
        let rows = stmt
            .query_map(params![address.trim().to_lowercase(), role_to_int(Role::Junk), role_to_int(Role::Trash), limit as i64], |r| Ok((r.get(17)?, Hit { message: summary(r)?, folder: r.get(16)? })))
            .map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Messages of `folder` that answer one the user sent: their In-Reply-To or References name a
    /// message of the Sent folder.
    pub fn replies_to_sent(&self, account: &str, folder: &str) -> Result<std::collections::HashSet<MessageId>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT m.message_id FROM messages m JOIN folders f ON f.account = m.account AND f.name = m.folder WHERE m.account = ?1 AND f.role = ?2 AND m.message_id != ''")
            .map_err(db)?;
        let sent: std::collections::HashSet<String> = stmt.query_map(params![account, role_to_int(Role::Sent)], |r| r.get(0)).map_err(db)?.collect::<Result<_, _>>().map_err(db)?;
        let mut stmt = conn.prepare("SELECT id, in_reply_to, refs FROM messages WHERE account = ?1 AND folder = ?2").map_err(db)?;
        let rows = stmt.query_map(params![account, folder], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).map_err(db)?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            let (id, in_reply_to, refs) = row.map_err(db)?;
            if in_reply_to.split_whitespace().chain(refs.split_whitespace()).any(|r| sent.contains(r)) {
                out.insert(MessageId(id as u64));
            }
        }
        Ok(out)
    }

    /// Every (display name, lower-case address) the account has received mail from, once each.
    pub fn senders(&self, account: &str) -> Result<Vec<(String, String)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT DISTINCT from_name, lower(from_addr) FROM messages WHERE account = ?1 AND from_addr != ''").map_err(db)?;
        let rows = stmt.query_map([account], |r| Ok((r.get(0)?, r.get(1)?))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Messages dated `since` or later whose file is not stored, newest first.
    pub fn pending(&self, account: &str, folder: &str, since: i64, limit: usize) -> Result<Vec<Pending>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT uid, size FROM messages WHERE account = ?1 AND folder = ?2 AND stored = 0 AND timestamp >= ?3 ORDER BY timestamp DESC, uid DESC LIMIT ?4")
            .map_err(db)?;
        let rows = stmt.query_map(params![account, folder, since, limit as i64], |r| Ok(Pending { uid: r.get::<_, i64>(0)? as u32, size: r.get::<_, i64>(1)? as u32 })).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// How many messages [`Store::pending`] would list in all.
    pub fn count_pending(&self, account: &str, folder: &str, since: i64) -> Result<u32, MailError> {
        let conn = self.conn();
        conn.query_row("SELECT COUNT(*) FROM messages WHERE account = ?1 AND folder = ?2 AND stored = 0 AND timestamp >= ?3", params![account, folder, since], |r| r.get::<_, i64>(0).map(|n| n as u32)).map_err(db)
    }

    /// Records that the message's file is in the vault.
    pub fn mark_stored(&self, account: &str, folder: &str, uid: u32) -> Result<(), MailError> {
        self.conn()
            .execute("UPDATE messages SET stored = 1 WHERE account = ?1 AND folder = ?2 AND uid = ?3", params![account, folder, i64::from(uid)])
            .map(|_| ())
            .map_err(db)
    }

    /// The date of the oldest cached message of a folder.
    pub fn oldest_timestamp(&self, account: &str, folder: &str) -> Result<Option<i64>, MailError> {
        let conn = self.conn();
        conn.query_row("SELECT MIN(timestamp) FROM messages WHERE account = ?1 AND folder = ?2", params![account, folder], |r| r.get(0)).map_err(db)
    }

    /// The UID of the `n`-th newest cached message (1 is the newest) of a folder.
    pub fn nth_newest_uid(&self, account: &str, folder: &str, n: u32) -> Result<Option<u32>, MailError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT uid FROM messages WHERE account = ?1 AND folder = ?2 ORDER BY uid DESC LIMIT 1 OFFSET ?3",
            params![account, folder, i64::from(n.saturating_sub(1))],
            |r| r.get::<_, i64>(0).map(|u| u as u32),
        )
        .optional()
        .map_err(db)
    }

    /// The name of the account's folder with this role, if it has one.
    pub fn folder_with_role(&self, account: &str, role: Role) -> Result<Option<String>, MailError> {
        let conn = self.conn();
        conn.query_row("SELECT name FROM folders WHERE account = ?1 AND role = ?2 LIMIT 1", params![account, role_to_int(role)], |r| r.get(0)).optional().map_err(db)
    }

    /// Where the message lives on the server.
    pub fn locate(&self, id: MessageId) -> Result<Option<Location>, MailError> {
        let conn = self.conn();
        conn.query_row("SELECT account, folder, uid FROM messages WHERE id = ?1", [id.0 as i64], |r| {
            Ok(Location { account: r.get(0)?, folder: r.get(1)?, uid: r.get::<_, i64>(2)? as u32 })
        })
        .optional()
        .map_err(db)
    }

    pub fn set_unread(&self, id: MessageId, unread: bool) -> Result<(), MailError> {
        self.conn().execute("UPDATE messages SET unread = ?2 WHERE id = ?1", params![id.0 as i64, i64::from(unread)]).map(|_| ()).map_err(db)
    }

    /// Adds label `keyword` to a message, or takes it off.
    pub fn set_label(&self, id: MessageId, keyword: &str, on: bool) -> Result<(), MailError> {
        let conn = self.conn();
        let labels: String = conn.query_row("SELECT labels FROM messages WHERE id = ?1", [id.0 as i64], |r| r.get(0)).optional().map_err(db)?.unwrap_or_default();
        let mut list: Vec<&str> = labels.split_whitespace().filter(|l| *l != keyword).collect();
        if on {
            list.push(keyword);
        }
        conn.execute("UPDATE messages SET labels = ?2 WHERE id = ?1", params![id.0 as i64, list.join(" ")]).map(|_| ()).map_err(db)
    }

    pub fn set_starred(&self, id: MessageId, starred: bool) -> Result<(), MailError> {
        self.conn().execute("UPDATE messages SET starred = ?2 WHERE id = ?1", params![id.0 as i64, i64::from(starred)]).map(|_| ()).map_err(db)
    }

    pub fn set_pinned(&self, id: MessageId, pinned: bool) -> Result<(), MailError> {
        self.conn().execute("UPDATE messages SET pinned = ?2 WHERE id = ?1", params![id.0 as i64, i64::from(pinned)]).map(|_| ()).map_err(db)
    }

    pub fn remove(&self, id: MessageId) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM messages WHERE id = ?1", [id.0 as i64]).map(|_| ()).map_err(db)
    }

    /// Keeps a one-line preview (the list shows only its start; the spam filter reads all of it), taken from the message text once it is downloaded.
    pub fn set_preview(&self, id: MessageId, text: &str) -> Result<(), MailError> {
        let preview: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect();
        self.conn().execute("UPDATE messages SET preview = ?2 WHERE id = ?1", params![id.0 as i64, preview]).map(|_| ()).map_err(db)
    }

    /// Keeps the words of a message's text for searching (at most the first 200 000 characters).
    pub fn index_body(&self, id: MessageId, text: &str) -> Result<(), MailError> {
        let text: String = text.chars().take(200_000).collect();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM body_fts WHERE rowid = ?1", [id.0 as i64]).map_err(db)?;
        tx.execute("INSERT INTO body_fts (rowid, body) VALUES (?1, ?2)", params![id.0 as i64, text]).map_err(db)?;
        tx.execute("UPDATE messages SET indexed = 1 WHERE id = ?1", [id.0 as i64]).map_err(db)?;
        tx.commit().map_err(db)
    }

    /// Messages whose file is stored but whose text is not searchable yet: (id, folder, uid).
    pub fn unindexed(&self, account: &str, limit: usize) -> Result<Vec<(MessageId, String, u32)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, folder, uid FROM messages WHERE account = ?1 AND stored = 1 AND indexed = 0 LIMIT ?2").map_err(db)?;
        let rows = stmt.query_map(params![account, limit as i64], |r| Ok((MessageId(r.get::<_, i64>(0)? as u64), r.get(1)?, r.get::<_, i64>(2)? as u32))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Empties the text index, as if the messages had been stored before searching existed. For tests.
    #[doc(hidden)]
    pub fn forget_index_for_tests(&self) {
        let conn = self.conn();
        conn.execute_batch("INSERT INTO body_fts (body_fts) VALUES ('delete-all'); UPDATE messages SET indexed = 0, preview = '';").expect("the index can be emptied");
    }

    /// The file of a message turned out to be missing: it is not stored after all, and not indexed.
    pub fn clear_stored(&self, id: MessageId) -> Result<(), MailError> {
        self.conn().execute("UPDATE messages SET stored = 0, indexed = 0 WHERE id = ?1", [id.0 as i64]).map(|_| ()).map_err(db)
    }

    /// Marks a stored message as searched, when it has no text to add.
    pub fn mark_indexed(&self, id: MessageId) -> Result<(), MailError> {
        self.conn().execute("UPDATE messages SET indexed = 1 WHERE id = ?1", [id.0 as i64]).map(|_| ()).map_err(db)
    }

    /// Messages of one account that match `query`, newest first. Words must all appear, in the sender,
    /// subject, preview or text of the message. `from:`, `subject:`, `in:<folder>`, `is:unread` and
    /// `has:attachment` narrow it down; a `"quoted phrase"` stays together. User input is never FTS syntax.
    pub fn search(&self, account: &str, query: &str, limit: usize) -> Result<Vec<Hit>, MailError> {
        let query = Query::parse(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut conditions = vec!["account = ?1".to_owned()];
        let mut args: Vec<String> = vec![account.to_owned()];
        let arg = |value: String, args: &mut Vec<String>| {
            args.push(value);
            format!("?{}", args.len())
        };
        for word in &query.words {
            let p = arg(fts_term(word), &mut args);
            conditions.push(format!("(id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH {p}) OR id IN (SELECT rowid FROM body_fts WHERE body_fts MATCH {p}))"));
        }
        for who in &query.from {
            let p = arg(format!("{{from_name from_addr}} : {}", fts_term(who)), &mut args);
            conditions.push(format!("id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH {p})"));
        }
        for who in &query.to {
            // The address list is plain text: % and _ are taken literally.
            let p = arg(format!("%{}%", who.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")), &mut args);
            conditions.push(format!("recipients LIKE {p} ESCAPE '\\'"));
        }
        if let Some(after) = query.after {
            let p = arg(after.to_string(), &mut args);
            conditions.push(format!("timestamp >= CAST({p} AS INTEGER)"));
        }
        if let Some(before) = query.before {
            let p = arg(before.to_string(), &mut args);
            conditions.push(format!("timestamp < CAST({p} AS INTEGER)"));
        }
        for what in &query.subject {
            let p = arg(format!("subject : {}", fts_term(what)), &mut args);
            conditions.push(format!("id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH {p})"));
        }
        for folder in &query.folders {
            let p = arg(folder.to_lowercase(), &mut args);
            conditions.push(format!("LOWER(folder) = {p}"));
        }
        if query.unread {
            conditions.push("unread = 1".to_owned());
        }
        if query.attachment {
            conditions.push("attach = 1".to_owned());
        }
        let sql = format!("SELECT {SUMMARY_COLUMNS}, folder FROM messages WHERE {} ORDER BY timestamp DESC LIMIT {}", conditions.join(" AND "), limit.min(1000));
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(db)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok(Hit { message: summary(r)?, folder: r.get(16)? })).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// The id of the message with this UID, if it is cached.
    pub fn id_of(&self, account: &str, folder: &str, uid: u32) -> Result<Option<MessageId>, MailError> {
        let conn = self.conn();
        conn.query_row("SELECT id FROM messages WHERE account = ?1 AND folder = ?2 AND uid = ?3", params![account, folder, i64::from(uid)], |r| {
            r.get::<_, i64>(0).map(|i| MessageId(i as u64))
        })
        .optional()
        .map_err(db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(uid: u32, subject: &str, timestamp: i64, unread: bool) -> Header {
        Header {
            uid,
            from_name: "Ada Lovelace".into(),
            from_addr: "ada@example.org".into(),
            subject: subject.into(),
            received: "2026-10-13 12:00".into(),
            timestamp,
            unread,
            ..Header::default()
        }
    }

    fn store() -> Store {
        let store = Store::in_memory().unwrap();
        store.set_folders("me", &[("INBOX".into(), Role::Inbox), ("Trash".into(), Role::Trash)]).unwrap();
        store
    }

    #[test]
    fn headers_come_back_newest_first_and_are_not_duplicated() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "old", 100, false), header(2, "new", 200, true)]).unwrap();
        store.add_headers("me", "INBOX", &[header(2, "new again", 200, false)]).unwrap();
        let list = store.messages("me", "INBOX").unwrap();
        assert_eq!(list.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(), ["new", "old"]);
        assert!(list[0].unread, "an existing UID keeps its row");
        assert_eq!(store.uid_range("me", "INBOX").unwrap(), Some((1, 2, 2)));
    }

    #[test]
    fn a_renamed_folder_takes_those_inside_it_and_their_messages_along() {
        let store = Store::in_memory().unwrap();
        let folders: Vec<(String, Role)> = ["INBOX", "INBOX/Receipts", "INBOX/Receipts/Old", "INBOX/Receipts.2025", "INBOX/ReceiptsX"].into_iter().map(|n| (n.to_owned(), Role::Other)).collect();
        store.set_folders("me", &folders).unwrap();
        store.set_delimiter("me", "INBOX/Receipts", Some('/')).unwrap();
        store.add_headers("me", "INBOX/Receipts/Old", &[header(1, "kept", 1, true)]).unwrap();
        let id = store.messages("me", "INBOX/Receipts/Old").unwrap()[0].id;
        let moved = store.rename_folder("me", "INBOX/Receipts", "INBOX/Bills", '/').unwrap();
        assert_eq!(moved, [("INBOX/Receipts".to_owned(), "INBOX/Bills".to_owned()), ("INBOX/Receipts/Old".to_owned(), "INBOX/Bills/Old".to_owned())]);
        let mut names: Vec<String> = store.folders("me").unwrap().into_iter().map(|f| f.name).collect();
        names.sort();
        // A dot is part of a name when the separator is "/", and a longer name is not inside a shorter one.
        assert_eq!(names, ["INBOX", "INBOX/Bills", "INBOX/Bills/Old", "INBOX/Receipts.2025", "INBOX/ReceiptsX"]);
        assert_eq!(store.messages("me", "INBOX/Bills/Old").unwrap()[0].id, id, "the same message, not a new one");
        assert_eq!(store.folders("me").unwrap().iter().find(|f| f.name == "INBOX/Bills").unwrap().delimiter, Some('/'));
        store.forget_folder("me", "INBOX/Bills/Old").unwrap();
        assert!(store.messages("me", "INBOX/Bills/Old").unwrap().is_empty());
        assert!(!store.folders("me").unwrap().iter().any(|f| f.name == "INBOX/Bills/Old"));
    }

    #[test]
    fn folders_report_unread_counts_and_drop_vanished_ones() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, true), header(2, "b", 2, true), header(3, "c", 3, false)]).unwrap();
        let folders = store.folders("me").unwrap();
        assert_eq!(folders.iter().find(|f| f.name == "INBOX").unwrap().unread, 2);
        store.set_folders("me", &[("INBOX".into(), Role::Inbox)]).unwrap();
        assert_eq!(store.folders("me").unwrap().len(), 1);
    }

    #[test]
    fn a_folder_reports_the_servers_total_and_how_many_files_are_stored() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, true), header(2, "b", 2, false)]).unwrap();
        let inbox = |s: &Store| s.folders("me").unwrap().into_iter().find(|f| f.name == "INBOX").unwrap();
        assert_eq!((inbox(&store).total, inbox(&store).stored), (2, 0), "before the server is asked: what is cached");
        store.set_total("me", "INBOX", 1_240).unwrap();
        store.mark_stored("me", "INBOX", 2).unwrap();
        let row = inbox(&store);
        assert_eq!((row.total, row.unread, row.stored), (1_240, 1, 1));
    }

    #[test]
    fn a_new_uidvalidity_empties_the_folder() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, false)]).unwrap();
        store.reset_folder("me", "INBOX", 7).unwrap();
        assert!(store.messages("me", "INBOX").unwrap().is_empty());
        assert_eq!(store.folder_state("me", "INBOX").unwrap(), FolderState { uidvalidity: Some(7), floor_uid: None });
    }

    #[test]
    fn ids_are_stable_and_locate_the_server_copy() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(42, "a", 1, true)]).unwrap();
        let id = store.id_of("me", "INBOX", 42).unwrap().unwrap();
        assert_eq!(store.locate(id).unwrap(), Some(Location { account: "me".into(), folder: "INBOX".into(), uid: 42 }));
        store.set_unread(id, false).unwrap();
        assert!(!store.messages("me", "INBOX").unwrap()[0].unread);
        store.remove(id).unwrap();
        assert_eq!(store.locate(id).unwrap(), None);
    }

    #[test]
    fn a_pin_is_kept_from_the_server_and_from_the_user() {
        let store = store();
        store.add_headers("me", "INBOX", &[Header { pinned: true, ..header(1, "a", 1, false) }, header(2, "b", 2, false)]).unwrap();
        let pinned = |s: &Store| s.messages("me", "INBOX").unwrap().iter().map(|m| (m.subject.clone(), m.pinned)).collect::<Vec<_>>();
        assert_eq!(pinned(&store), [("b".to_owned(), false), ("a".to_owned(), true)]);
        assert_eq!(store.flags_since("me", "INBOX", 0).unwrap().iter().find(|f| f.0 == 1), Some(&(1, Flags { pinned: true, ..Flags::default() })));
        store.set_flags_by_uid("me", "INBOX", &[(1, Flags { unread: true, pinned: false, starred: true, labels: "$label1 clients".into() })]).unwrap();
        let a = store.messages("me", "INBOX").unwrap().into_iter().find(|m| m.subject == "a").unwrap();
        assert!(a.starred && !a.pinned && a.labels == ["$label1", "clients"]);
        let id = store.id_of("me", "INBOX", 2).unwrap().unwrap();
        store.set_pinned(id, true).unwrap();
        assert_eq!(pinned(&store), [("b".to_owned(), true), ("a".to_owned(), false)]);
        let hit = &store.search("me", "subject:b", 10).unwrap()[0];
        assert!(hit.message.pinned && hit.folder == "INBOX", "search reads the pin and the folder in their places");
    }

    #[test]
    fn pending_lists_newest_first_inside_the_window_and_shrinks_as_files_are_stored() {
        let store = store();
        let mut old = header(1, "old", 100, false);
        old.size = 10;
        let mut new = header(2, "new", 900, false);
        new.size = 20;
        store.add_headers("me", "INBOX", &[old, new, header(3, "newer", 1000, false)]).unwrap();
        let pending = store.pending("me", "INBOX", 500, 10).unwrap();
        assert_eq!(pending.iter().map(|p| p.uid).collect::<Vec<_>>(), [3, 2], "the old one is outside the window");
        assert_eq!(pending[1].size, 20);
        assert_eq!(store.count_pending("me", "INBOX", 500).unwrap(), 2);
        assert_eq!(store.count_pending("me", "INBOX", i64::MIN).unwrap(), 3);
        store.mark_stored("me", "INBOX", 3).unwrap();
        assert_eq!(store.pending("me", "INBOX", 500, 10).unwrap().iter().map(|p| p.uid).collect::<Vec<_>>(), [2]);
        assert_eq!(store.oldest_timestamp("me", "INBOX").unwrap(), Some(100));
        assert_eq!(store.pending("me", "INBOX", 500, 1).unwrap().len(), 1, "the limit applies");
    }

    #[test]
    fn a_database_from_before_the_download_columns_is_upgraded() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        let store = Store::with(conn, None, None).unwrap();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, false)]).unwrap();
        assert_eq!(store.count_pending("me", "INBOX", 0).unwrap(), 1);
    }

    #[test]
    fn the_paperclip_flag_comes_back_with_the_list() {
        let store = store();
        let mut with = header(1, "with", 2, false);
        with.has_attachments = true;
        store.add_headers("me", "INBOX", &[with, header(2, "without", 1, false)]).unwrap();
        let list = store.messages("me", "INBOX").unwrap();
        assert_eq!(list.iter().map(|m| m.has_attachments).collect::<Vec<_>>(), [true, false]);
    }

    #[test]
    fn rows_of_unknown_attachment_status_are_found_and_settled() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, false), header(2, "b", 2, false)]).unwrap();
        // Rows written by this version already know their answer.
        assert!(store.unknown_attachments("me", "INBOX", 10).unwrap().is_empty());
        // A row from before the column existed does not.
        store.conn().execute("UPDATE messages SET attach_known = 0 WHERE uid = 1", []).unwrap();
        assert_eq!(store.unknown_attachments("me", "INBOX", 10).unwrap(), [1]);
        store.set_attachments("me", "INBOX", 1, true).unwrap();
        assert!(store.unknown_attachments("me", "INBOX", 10).unwrap().is_empty());
        assert!(store.messages("me", "INBOX").unwrap().iter().find(|m| m.subject == "a").unwrap().has_attachments);
    }

    #[test]
    fn contacts_are_learned_once_from_sent_mail_and_named_by_their_own() {
        let store = store();
        store.set_folders("me", &[("INBOX".into(), Role::Inbox), ("Sent".into(), Role::Sent), ("Junk".into(), Role::Junk)]).unwrap();
        let sent = |uid, id: &str, to: &str, at| Header { message_id: id.into(), recipients: to.into(), ..header(uid, "s", at, false) };
        let from = |uid, name: &str, address: &str| Header { from_name: name.into(), from_addr: address.into(), ..header(uid, "hi", 5, false) };
        store.add_headers("me", "INBOX", &[from(1, "Nicolò Rossi", "nico@x.org")]).unwrap();
        store.add_headers("me", "Sent", &[sent(1, "<a@me>", "nico@x.org bob@y.org", 10), sent(2, "<b@me>", "bob@y.org bob@y.org", 20)]).unwrap();
        let names = |store: &Store, account| store.contacts(account).unwrap().into_iter().map(|c| (c.name, c.address)).collect::<Vec<_>>();
        assert_eq!(names(&store, "me"), [(String::new(), "bob@y.org".to_owned()), ("Nicolò Rossi".to_owned(), "nico@x.org".to_owned())], "bob twice, nico once, named by mail that came first");
        // The folder downloaded again counts nothing twice.
        store.reset_folder("me", "Sent", 2).unwrap();
        store.add_headers("me", "Sent", &[sent(7, "<a@me>", "nico@x.org bob@y.org", 10), sent(8, "<b@me>", "bob@y.org", 20)]).unwrap();
        let count = |address: &str| store.conn().query_row("SELECT SUM(sent) FROM contact_accounts WHERE address = ?1", [address], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!((count("bob@y.org"), count("nico@x.org")), (2, 1));
        // A name from later mail, not from Spam.
        store.add_headers("me", "INBOX", &[from(2, "Bob Brown", "Bob@y.org")]).unwrap();
        store.add_headers("me", "Junk", &[Header { timestamp: 99, ..from(1, "Free Money", "nico@x.org") }]).unwrap();
        assert_eq!(names(&store, "me")[0].0, "Bob Brown");
        assert_eq!(names(&store, "me")[1].0, "Nicolò Rossi");
        // Writing from another account: its own people come first there.
        store.set_folders("work", &[("Sent".into(), Role::Sent)]).unwrap();
        store.add_headers("work", "Sent", &[sent(1, "<c@work>", "nico@x.org", 30)]).unwrap();
        assert_eq!(names(&store, "work")[0].1, "nico@x.org");
        assert_eq!(names(&store, "me")[0].1, "bob@y.org");
        // A Sent row read before recipients were kept is learned when they are.
        store.add_headers("work", "Sent", &[Header { message_id: "<d@work>".into(), ..header(2, "s", 40, false) }]).unwrap();
        store.set_recipients("work", "Sent", 2, "erin@z.org").unwrap();
        assert!(names(&store, "work").iter().any(|(_, a)| a == "erin@z.org"));
        // Accents and case aside, by the start of any word.
        let nico = Contact::new("Nicolò Rossi", "nico@x.org");
        assert!(nico.matches("nicolo") && nico.matches("ROSSI ni") && nico.matches("x.org") && !nico.matches("icol") && !nico.matches(" "));
        assert_eq!(fold("Ærø-Straße"), " aero strasse");
        // Removing an account forgets whom only it wrote to.
        store.forget_account("work").unwrap();
        assert!(!names(&store, "me").iter().any(|(_, a)| a == "erin@z.org"));
        assert!(names(&store, "me").iter().any(|(_, a)| a == "nico@x.org"), "also written to from this one");
        // An archive from before contacts learns them from what it holds.
        {
            let conn = store.conn();
            conn.execute_batch("DROP TABLE contacts; DROP TABLE contact_accounts; DROP TABLE contacts_learned; PRAGMA user_version = 13;").unwrap();
            migrate(&conn).unwrap();
        }
        assert_eq!(names(&store, "me"), [("Bob Brown".to_owned(), "bob@y.org".to_owned()), ("Nicolò Rossi".to_owned(), "nico@x.org".to_owned())]);
        assert_eq!(count("bob@y.org"), 2);
    }

    #[test]
    fn a_saved_contact_keeps_its_name_and_notes_and_has_a_history() {
        let store = store();
        store.set_folders("me", &[("INBOX".into(), Role::Inbox), ("Sent".into(), Role::Sent), ("Junk".into(), Role::Junk)]).unwrap();
        let from = |uid, at| Header { from_name: "Bob B".into(), from_addr: "Bob@y.org".into(), ..header(uid, "from bob", at, false) };
        store.add_headers("me", "INBOX", &[from(1, 10)]).unwrap();
        store.add_headers("me", "Junk", &[from(2, 50)]).unwrap();
        store.add_headers("me", "Sent", &[Header { message_id: "<a@me>".into(), recipients: "bob@y.org carl@z.org".into(), ..header(1, "to bob", 20, false) }]).unwrap();
        let card = store.contact_card("BOB@y.org").unwrap().unwrap();
        assert_eq!((card.name.as_str(), card.saved, card.sent, card.last_sent), ("Bob B", false, 1, Some(20)), "a suggestion, written to once");
        assert_eq!(store.contact_card("nobody@x.org").unwrap(), None);
        // His mail both ways, newest first, Spam left out.
        let with: Vec<String> = store.messages_with("bob@y.org", 10).unwrap().into_iter().map(|(_, h)| h.message.subject).collect();
        assert_eq!(with, ["to bob", "from bob"]);
        assert!(store.messages_with("arl@z.org", 10).unwrap().is_empty(), "a whole address, not part of one");
        // Saved: his own mail no longer renames him.
        store.save_contact("bob@y.org", "Robert", "met at the fair").unwrap();
        store.add_headers("me", "INBOX", &[Header { from_name: "Bobby".into(), ..from(3, 30) }]).unwrap();
        let card = store.contact_card("bob@y.org").unwrap().unwrap();
        assert_eq!((card.name.as_str(), card.saved, card.notes.as_str()), ("Robert", true, "met at the fair"));
        assert!(Contact::new("Robert", "bob@y.org").matches("rob"));
        assert_eq!(store.contacts("me").unwrap().iter().find(|c| c.address == "bob@y.org").unwrap().name, "Robert");
        // Someone never written to can be saved too, and leaves entirely when taken out.
        store.save_contact("dora@w.org", "Dora", "").unwrap();
        assert_eq!(store.saved_contacts().unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Dora", "Robert"]);
        store.unsave_contact("dora@w.org").unwrap();
        assert_eq!(store.contact_card("dora@w.org").unwrap(), None);
        store.unsave_contact("bob@y.org").unwrap();
        let card = store.contact_card("bob@y.org").unwrap().unwrap();
        assert!(!card.saved && card.notes.is_empty(), "back to a suggestion: written to");
    }

    #[test]
    fn address_book_cards_wait_their_turn_and_name_the_suggestions() {
        let store = store();
        let fields = |name: &str, emails: &[&str], phones: &[&str]| CardFields { name: name.into(), emails: emails.iter().map(|e| (*e).to_owned()).collect(), phones: phones.iter().map(|p| (*p).to_owned()).collect(), note: String::new() };
        store.put_card("book", "/a.vcf", "\"1\"", "A", &fields("Ada Lovelace", &["ada@x.org"], &[])).unwrap();
        store.put_card("book", "/m.vcf", "\"2\"", "M", &fields("Mario Rossi", &[], &["+39 02 1234"])).unwrap();
        assert_eq!(store.address_book().unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Ada Lovelace", "Mario Rossi"], "a phone-only card too");
        assert_eq!(store.card_with_email("ADA@x.org").unwrap().unwrap().href, "/a.vcf");
        assert!(store.card_with_email("da@x.org").unwrap().is_none(), "a whole address");
        // In the suggestions, by the address book's name, even never written to.
        assert!(store.contacts("me").unwrap().iter().any(|c| c.address == "ada@x.org" && c.name == "Ada Lovelace"));
        // Made here: deleted at once, never having reached the server; changed after sync: waits.
        store.add_card("book", "/n.vcf", "N", &fields("New", &["new@x.org"], &[])).unwrap();
        let new = store.card_with_email("new@x.org").unwrap().unwrap();
        assert_eq!(new.state, CardState::New);
        store.delete_card(new.id).unwrap();
        assert!(store.card(new.id).unwrap().is_none());
        let ada = store.card_with_email("ada@x.org").unwrap().unwrap();
        store.edit_card(ada.id, "A2", &fields("Augusta", &["ada@x.org"], &[])).unwrap();
        assert_eq!(store.card(ada.id).unwrap().unwrap().state, CardState::Changed);
        store.delete_card(ada.id).unwrap();
        assert_eq!(store.card(ada.id).unwrap().unwrap().state, CardState::Deleted, "kept until the server deletes it");
        assert!(!store.address_book().unwrap().iter().any(|c| c.id == ada.id), "but not shown");
        store.forget_book("book").unwrap();
        assert!(store.cards("book").unwrap().is_empty());
    }

    #[test]
    fn the_latest_certificate_a_sender_signed_with_is_kept() {
        let store = store();
        store.learn_peer_cert("Ada@X.org", b"old", 1).unwrap();
        store.learn_peer_cert("ada@x.org", b"renewed", 2).unwrap();
        assert_eq!(store.peer_cert("ada@x.org").unwrap().as_deref(), Some(&b"renewed"[..]), "an authority vouched for both: the newer one wins");
    }

    #[test]
    fn the_first_key_seen_for_a_sender_stays() {
        let store = store();
        assert_eq!(store.peer_key("ada@x.org").unwrap(), None);
        store.learn_peer_key("Ada@X.org", b"first", 1).unwrap();
        store.learn_peer_key("ada@x.org", b"second", 2).unwrap();
        assert_eq!(store.peer_key("ada@x.org").unwrap().as_deref(), Some(&b"first"[..]), "a later message cannot replace it");
    }

    #[test]
    fn a_preview_is_one_clean_line() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, false)]).unwrap();
        let id = store.id_of("me", "INBOX", 1).unwrap().unwrap();
        store.set_preview(id, "Hello\n\n  world").unwrap();
        assert_eq!(store.messages("me", "INBOX").unwrap()[0].preview, "Hello world");
        assert!(store.search("me", "world", 10).unwrap().len() == 1, "the preview is searchable");
        store.set_preview(id, &"word ".repeat(1000)).unwrap();
        assert_eq!(store.messages("me", "INBOX").unwrap()[0].preview.chars().count(), PREVIEW_CHARS, "a long text is cut");
    }

    #[test]
    fn an_old_database_loses_its_bodies_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE bodies (id INTEGER PRIMARY KEY, data TEXT); PRAGMA user_version = 1;").unwrap();
        let store = Store::with(conn, None, None).unwrap();
        let tables: i64 = store.conn().query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'bodies'", [], |r| r.get(0)).unwrap();
        assert_eq!(tables, 0);
    }

    #[test]
    fn search_matches_words_in_any_order_and_treats_input_as_text() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "Quarterly budget review", 1, false), header(2, "Lunch", 2, false)]).unwrap();
        assert_eq!(store.search("me", "budget quart", 10).unwrap().len(), 1);
        assert_eq!(store.search("me", "lovelace", 10).unwrap().len(), 2, "sender names are indexed");
        assert!(store.search("me", "budget AND OR \" NEAR(", 10).unwrap().is_empty());
        store.remove_uids("me", "INBOX", &[1]).unwrap();
        assert!(store.search("me", "budget", 10).unwrap().is_empty(), "deleted messages leave the index");
    }

    #[test]
    fn forgetting_an_account_removes_only_its_data() {
        let store = store();
        store.set_folders("you", &[("INBOX".into(), Role::Inbox)]).unwrap();
        store.add_headers("me", "INBOX", &[header(1, "mine", 1, false)]).unwrap();
        store.add_headers("you", "INBOX", &[header(1, "yours", 1, false)]).unwrap();
        store.forget_account("me").unwrap();
        assert!(store.messages("me", "INBOX").unwrap().is_empty());
        assert!(store.folders("me").unwrap().is_empty());
        assert_eq!(store.messages("you", "INBOX").unwrap().len(), 1);
        assert!(store.search("me", "mine", 10).unwrap().is_empty());
    }

    #[test]
    fn fifty_thousand_messages_list_and_search_quickly() {
        let store = store();
        let headers: Vec<Header> = (1..=50_000).map(|uid| header(uid, &format!("report number {uid} budget"), i64::from(uid), uid % 7 == 0)).collect();
        for chunk in headers.chunks(1_000) {
            store.add_headers("me", "INBOX", chunk).unwrap();
        }
        let started = std::time::Instant::now();
        assert_eq!(store.messages("me", "INBOX").unwrap().len(), 50_000);
        let list = started.elapsed();
        let started = std::time::Instant::now();
        assert_eq!(store.search("me", "report 4999", 50).unwrap().len(), 11, "4999 and the ten numbers that start with it");
        let search = started.elapsed();
        eprintln!("list of 50k: {list:?}, search: {search:?}");
        // Search over 50k messages in 500 ms. Generous bounds so a slow CI box does not flake.
        assert!(search < std::time::Duration::from_millis(500), "search took {search:?}");
        assert!(list < std::time::Duration::from_secs(2), "list took {list:?}");
    }

    fn found(store: &Store, query: &str) -> Vec<String> {
        store.search("me", query, 50).unwrap().into_iter().map(|h| h.message.subject).collect()
    }

    #[test]
    fn to_before_and_after_narrow_the_results() {
        let store = store();
        let day = |d: &str| day_start(d).unwrap();
        let sent = |uid, subject: &str, to: &str, at| Header { recipients: to.into(), ..header(uid, subject, at, false) };
        store
            .add_headers("me", "INBOX", &[sent(1, "September note", "grace@navy.example", day("2026-09-15") + 3600), sent(2, "October note", "ada@example.org bob@x_y.example", day("2026-10-02") + 60)])
            .unwrap();
        assert_eq!(found(&store, "to:grace"), ["September note"]);
        assert_eq!(found(&store, "to:x_y"), ["October note"], "_ is a letter here, not a wildcard");
        assert!(found(&store, "to:x%y").is_empty());
        assert_eq!(found(&store, "after:2026-10-01"), ["October note"]);
        assert_eq!(found(&store, "before:2026-10 note"), ["September note"], "a month starts on its first day");
        assert_eq!(found(&store, "after:2026-09-15 before:2026-09-16"), ["September note"], "after: includes its day, before: does not");
        assert!(found(&store, "after:yesterday").is_empty(), "not a date: an ordinary word");
    }

    #[test]
    fn the_text_of_a_message_is_searchable_once_indexed_and_leaves_with_the_message() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "Plan", 1, false), header(2, "Lunch", 2, false)]).unwrap();
        let id = store.id_of("me", "INBOX", 1).unwrap().unwrap();
        assert!(found(&store, "gazpacho").is_empty());
        store.index_body(id, "We will serve gazpacho on Friday").unwrap();
        assert_eq!(found(&store, "gazpacho"), ["Plan"]);
        assert_eq!(found(&store, "gazp plan"), ["Plan"], "words from the text and the subject together");
        assert!(found(&store, "gazpacho lunch").is_empty(), "every word has to match");
        // Indexing again replaces, it does not add.
        store.index_body(id, "Now it is paella").unwrap();
        assert!(found(&store, "gazpacho").is_empty());
        assert_eq!(found(&store, "paella"), ["Plan"]);
        store.remove_uids("me", "INBOX", &[1]).unwrap();
        assert!(found(&store, "paella").is_empty(), "gone from the index with the message");
    }

    #[test]
    fn stored_messages_not_yet_in_the_index_are_listed_until_indexed() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, false), header(2, "b", 2, false)]).unwrap();
        assert!(store.unindexed("me", 10).unwrap().is_empty(), "no file yet, nothing to index");
        store.mark_stored("me", "INBOX", 1).unwrap();
        store.mark_stored("me", "INBOX", 2).unwrap();
        let waiting = store.unindexed("me", 10).unwrap();
        assert_eq!(waiting.iter().map(|(_, f, u)| (f.as_str(), *u)).collect::<Vec<_>>().len(), 2);
        store.index_body(waiting[0].0, "text").unwrap();
        store.mark_indexed(waiting[1].0).unwrap();
        assert!(store.unindexed("me", 10).unwrap().is_empty());
    }

    #[test]
    fn operators_narrow_a_search() {
        let store = store();
        let mut ada = header(1, "Budget review", 3, true);
        ada.has_attachments = true;
        let mut grace = header(2, "Budget notes", 2, false);
        grace.from_name = "Grace Hopper".into();
        grace.from_addr = "grace@navy.example".into();
        store.add_headers("me", "INBOX", &[ada, grace]).unwrap();
        store.add_headers("me", "Archive", &[header(3, "Old budget", 1, false)]).unwrap();
        assert_eq!(found(&store, "budget"), ["Budget review", "Budget notes", "Old budget"], "newest first");
        assert_eq!(found(&store, "from:grace"), ["Budget notes"]);
        assert_eq!(found(&store, "from:navy.example budget"), ["Budget notes"]);
        assert_eq!(found(&store, "subject:review"), ["Budget review"]);
        assert_eq!(found(&store, "is:unread"), ["Budget review"]);
        assert_eq!(found(&store, "has:attachment budget"), ["Budget review"]);
        assert_eq!(found(&store, "in:archive budget"), ["Old budget"]);
        assert_eq!(found(&store, "in:all budget").len(), found(&store, "budget").len(), "in:all is every folder");
        assert_eq!(found(&store, "\"budget notes\""), ["Budget notes"], "a phrase stays together");
        assert!(found(&store, "from:").len() <= 3, "an empty operator is just text");
        let hits = store.search("me", "in:archive", 10).unwrap();
        assert_eq!(hits[0].folder, "Archive");
    }

    #[test]
    fn search_text_is_never_syntax() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "Plain", 1, false)]).unwrap();
        for hostile in ["\"", "NEAR(", "a AND OR b", "* *", "subject:\"", "(((", "from:{x}", "'; DROP TABLE messages; --"] {
            assert!(store.search("me", hostile, 10).is_ok(), "{hostile}");
        }
        assert_eq!(found(&store, "plain"), ["Plain"], "and the table is still there");
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beavermail-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("mail.sqlite3")
    }

    /// Everything the files next to the database hold, as one byte string.
    fn raw_bytes(path: &Path) -> Vec<u8> {
        ["", "-wal", "-shm"].iter().flat_map(|suffix| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            std::fs::read(PathBuf::from(name)).unwrap_or_default()
        }).collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn fill(store: &Store) {
        store.set_folders("me", &[("INBOX".into(), Role::Inbox)]).unwrap();
        store.add_headers("me", "INBOX", &[header(1, "Quarterly zeppelin budget", 5, true)]).unwrap();
        let id = store.id_of("me", "INBOX", 1).unwrap().unwrap();
        store.index_body(id, "the quokka said hello").unwrap();
    }

    #[test]
    fn an_encrypted_index_is_unreadable_without_its_key_and_still_searchable_with_it() {
        let path = temp_file("encrypted");
        let key = [7u8; 32];
        fill(&Store::open(&path, Some(&key)).unwrap());
        let raw = raw_bytes(&path);
        assert!(!contains(&raw, b"zeppelin") && !contains(&raw, b"quokka") && !contains(&raw, b"ada@example.org"), "no plain text on disk");
        assert!(matches!(Store::open(&path, None), Err(MailError::Auth(_))), "no key");
        assert!(matches!(Store::open(&path, Some(&[8u8; 32])), Err(MailError::Auth(_))), "the wrong key");
        let again = Store::open(&path, Some(&key)).unwrap();
        assert_eq!(found(&again, "zeppelin").len(), 1, "the subject index");
        assert_eq!(found(&again, "quokka").len(), 1, "the text index");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_plain_index_is_encrypted_when_opened_with_a_key_and_loses_nothing() {
        let path = temp_file("convert");
        fill(&Store::open(&path, None).unwrap());
        assert!(contains(&raw_bytes(&path), b"zeppelin"), "plain to begin with");
        let key = [9u8; 32];
        let store = Store::open(&path, Some(&key)).unwrap();
        assert!(!contains(&raw_bytes(&path), b"zeppelin"), "encrypted now");
        assert_eq!(found(&store, "zeppelin").len(), 1);
        assert_eq!(found(&store, "quokka").len(), 1);
        assert_eq!(store.folders("me").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rekeying_changes_the_key_and_can_remove_the_encryption() {
        let path = temp_file("rekey");
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let store = Store::open(&path, Some(&a)).unwrap();
        fill(&store);
        store.rekey(Some(&b)).unwrap();
        assert!(matches!(Store::open(&path, Some(&a)), Err(MailError::Auth(_))), "the old key is dead");
        assert_eq!(found(&Store::open(&path, Some(&b)).unwrap(), "quokka").len(), 1);
        store.rekey(None).unwrap();
        assert!(contains(&raw_bytes(&path), b"zeppelin"), "plain again");
        assert_eq!(found(&store, "quokka").len(), 1, "the open store keeps working through the change");
        store.rekey(Some(&a)).unwrap();
        assert!(!contains(&raw_bytes(&path), b"zeppelin"));
        assert_eq!(found(&Store::open(&path, Some(&a)).unwrap(), "zeppelin").len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_locked_stand_in_is_empty_until_unlocked_and_a_reset_starts_over() {
        let path = temp_file("locked");
        let key = [3u8; 32];
        fill(&Store::open(&path, Some(&key)).unwrap());
        let store = Store::locked(&path).unwrap();
        assert!(store.is_locked() && store.folders("me").unwrap().is_empty());
        assert!(store.unlock(Some(&[4u8; 32])).is_err(), "the wrong key leaves it locked");
        assert!(store.is_locked());
        store.unlock(Some(&key)).unwrap();
        assert!(!store.is_locked());
        assert_eq!(found(&store, "zeppelin").len(), 1);
        // The key is lost: start over with a new one, and the old content is gone for good.
        store.reset(Some(&[5u8; 32])).unwrap();
        assert!(store.folders("me").unwrap().is_empty());
        assert!(!contains(&raw_bytes(&path), b"zeppelin"));
        assert!(Store::open(&path, Some(&[5u8; 32])).is_ok());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn dates_are_shown_in_local_time_from_the_timestamp() {
        let store = store();
        // Two messages written in different zones: the sender's clock text is not what is shown.
        store.add_headers("me", "INBOX", &[Header { received: "2026-10-14 16:41".into(), ..header(1, "a", 1_760_446_860, false) }, header(2, "b", 0, false)]).unwrap();
        let list = store.messages("me", "INBOX").unwrap();
        assert_eq!(list[0].received, local_time(1_760_446_860).unwrap());
        assert_eq!(list[1].received, "2026-10-13 12:00", "no date: the stored text");
    }

    #[test]
    fn other_accounts_do_not_see_each_others_mail() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "mine", 1, false)]).unwrap();
        assert!(store.messages("you", "INBOX").unwrap().is_empty());
        assert!(store.search("you", "mine", 10).unwrap().is_empty());
    }

    #[test]
    fn correspondents_are_the_recipients_of_sent_mail() {
        let store = store();
        store.set_folders("me", &[("INBOX".into(), Role::Inbox), ("Sent".into(), Role::Sent)]).unwrap();
        let sent = |uid, to: &str| Header { recipients: to.into(), ..header(uid, "s", 1, false) };
        store.add_headers("me", "Sent", &[sent(1, "bob@x.org carol@y.org"), sent(2, "")]).unwrap();
        store.add_headers("me", "INBOX", &[sent(1, "me@z.org")]).unwrap();
        let known = store.correspondents().unwrap();
        assert_eq!(known.len(), 2);
        assert!(known.contains("bob@x.org") && known.contains("carol@y.org") && !known.contains("me@z.org"));
        // Written to from another account counts too.
        store.set_folders("work", &[("Sent".into(), Role::Sent)]).unwrap();
        store.add_headers("work", "Sent", &[sent(1, "erin@w.org")]).unwrap();
        assert!(store.correspondents().unwrap().contains("erin@w.org"));
        // A row from before recipients were kept is read later, then counts.
        store.conn().execute("UPDATE messages SET recipients = NULL WHERE folder = 'Sent' AND uid = 2", []).unwrap();
        assert_eq!(store.unknown_recipients("me", "Sent", 10).unwrap(), [2]);
        store.set_recipients("me", "Sent", 2, "dan@x.org").unwrap();
        assert!(store.correspondents().unwrap().contains("dan@x.org"));
        store.add_headers("me", "Sent", &[Header { recipients: "ada@example.org".into(), ..header(6, "s", 9, false) }, Header { recipients: "ada@example.org".into(), ..header(7, "s", 1, false) }]).unwrap();
        let contacts = store.contacts("me").unwrap();
        assert_eq!(contacts[0], Contact::new("Ada Lovelace", "ada@example.org"), "written to most, named as her mail named her before: {contacts:?}");
        let rest: Vec<&str> = contacts[1..].iter().map(|c| c.address.as_str()).collect();
        assert_eq!(rest, ["bob@x.org", "carol@y.org", "dan@x.org", "erin@w.org"], "written to once on the same day: by address");
        assert!(!contacts.iter().any(|c| c.address == "me@z.org"), "only people written to");
        assert_eq!(store.senders("me").unwrap(), [("Ada Lovelace".to_owned(), "ada@example.org".to_owned())]);
        let id = store.id_of("me", "INBOX", 1).unwrap().unwrap();
        assert_eq!(store.headers(id).unwrap(), Some(Vec::new()));
        let mine = Header { message_id: "a@me".into(), ..header(3, "s", 1, false) };
        let answer = Header { in_reply_to: "a@me".into(), ..header(4, "re", 2, false) };
        let deeper = Header { references: "x@y a@me".into(), ..header(5, "re", 3, false) };
        store.add_headers("me", "Sent", &[mine]).unwrap();
        store.add_headers("me", "INBOX", &[answer, deeper]).unwrap();
        store.set_verdicts(&[(id, "held".into())]).unwrap();
        assert_eq!(store.verdicts("me", "INBOX").unwrap(), [(id, "held".to_owned())]);
        store.clear_verdicts("me", "INBOX").unwrap();
        assert!(store.verdicts("me", "INBOX").unwrap().is_empty());
        store.add_example("me", "a@x", true, "korean gelatin", "spam.io").unwrap();
        store.add_example("me", "a@x", false, "korean gelatin", "spam.io").unwrap();
        assert_eq!(store.examples("me").unwrap(), [("a@x".to_owned(), false, "korean gelatin".to_owned(), "spam.io".to_owned())], "a changed mind replaces");
        store.clear_examples().unwrap();
        assert!(store.examples("me").unwrap().is_empty());
        let replies = store.replies_to_sent("me", "INBOX").unwrap();
        assert_eq!(replies.len(), 2);
        assert!(!replies.contains(&id));
        store.conn().execute("UPDATE messages SET headers = NULL WHERE id = ?1", [id.0 as i64]).unwrap();
        assert_eq!((store.headers(id).unwrap(), store.unknown_headers("me", "INBOX", 10).unwrap()), (None, vec![1]));
        store.set_headers("me", "INBOX", 1, b"From: a@b").unwrap();
        assert_eq!(store.headers(id).unwrap().as_deref(), Some(&b"From: a@b"[..]));
    }
}
