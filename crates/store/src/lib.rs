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

/// A message whose file has not been downloaded yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub uid: u32,
    pub size: u32,
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
        message_id: row.get(9)?,
    })
}

/// A search as the user typed it, split into what it asks for.
#[derive(Default, Debug, PartialEq, Eq)]
struct Query {
    words: Vec<String>,
    from: Vec<String>,
    subject: Vec<String>,
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
                Some(("subject", v)) if !v.is_empty() => query.subject.push(v.to_owned()),
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

const SUMMARY_COLUMNS: &str = "id, CASE WHEN from_name = '' THEN from_addr ELSE from_name END, from_addr, subject, preview, received, timestamp, unread, attach, message_id";

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
                        (SELECT COUNT(*) FROM messages m WHERE m.account = f.account AND m.folder = f.name AND m.stored = 1)
                 FROM folders f WHERE f.account = ?1",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([account], |r| {
                Ok(FolderRow { name: r.get(0)?, role: role_from_int(r.get(1)?), unread: r.get::<_, i64>(2)? as u32, total: r.get::<_, i64>(3)? as u32, stored: r.get::<_, i64>(4)? as u32 })
            })
            .map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Remembers how many messages the server says a folder holds.
    pub fn set_total(&self, account: &str, folder: &str, total: u32) -> Result<(), MailError> {
        self.conn().execute("UPDATE folders SET total = ?3 WHERE account = ?1 AND name = ?2", params![account, folder, i64::from(total)]).map(|_| ()).map_err(db)
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

    /// Adds headers; a UID already in the archive keeps its row (and its body and preview).
    pub fn add_headers(&self, account: &str, folder: &str, headers: &[Header]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR IGNORE INTO messages
                     (account, folder, uid, from_name, from_addr, subject, received, timestamp, unread, message_id, in_reply_to, refs, size, attach, attach_known, recipients, headers)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1, ?15, ?16)",
                )
                .map_err(db)?;
            for h in headers {
                stmt.execute(params![
                    account, folder, i64::from(h.uid), h.from_name, h.from_addr, h.subject, h.received, h.timestamp, i64::from(h.unread),
                    h.message_id, h.in_reply_to, h.references, i64::from(h.size), i64::from(h.has_attachments), h.recipients, h.raw
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
    pub fn flags_since(&self, account: &str, folder: &str, from: u32) -> Result<Vec<(u32, bool)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT uid, unread FROM messages WHERE account = ?1 AND folder = ?2 AND uid >= ?3").map_err(db)?;
        let rows = stmt.query_map(params![account, folder, i64::from(from)], |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? != 0))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    pub fn set_unread_by_uid(&self, account: &str, folder: &str, uids: &[(u32, bool)]) -> Result<(), MailError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db)?;
        for (uid, unread) in uids {
            tx.execute(
                "UPDATE messages SET unread = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, i64::from(*uid), i64::from(*unread)],
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

    /// Records the To and Cc addresses of a message (lower case, separated by spaces).
    pub fn set_recipients(&self, account: &str, folder: &str, uid: u32, recipients: &str) -> Result<(), MailError> {
        self.conn()
            .execute("UPDATE messages SET recipients = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3", params![account, folder, i64::from(uid), recipients])
            .map(|_| ())
            .map_err(db)
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

    /// The training examples of an account: (message id, spam, text, sender domain).
    pub fn examples(&self, account: &str) -> Result<Vec<(String, bool, String, String)>, MailError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT message_id, spam, text, domain FROM examples WHERE account = ?1").map_err(db)?;
        let rows = stmt.query_map([account], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?, r.get(3)?))).map_err(db)?;
        rows.collect::<Result<_, _>>().map_err(db)
    }

    /// Everyone the user has written to from this account: the recipients of its Sent folder, lower case.
    pub fn correspondents(&self, account: &str) -> Result<std::collections::HashSet<String>, MailError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT m.recipients FROM messages m JOIN folders f ON f.account = m.account AND f.name = m.folder WHERE m.account = ?1 AND f.role = ?2 AND m.recipients IS NOT NULL")
            .map_err(db)?;
        let rows = stmt.query_map(params![account, role_to_int(Role::Sent)], |r| r.get::<_, String>(0)).map_err(db)?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.extend(row.map_err(db)?.split_whitespace().map(str::to_owned));
        }
        Ok(out)
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

    pub fn remove(&self, id: MessageId) -> Result<(), MailError> {
        self.conn().execute("DELETE FROM messages WHERE id = ?1", [id.0 as i64]).map(|_| ()).map_err(db)
    }

    /// Keeps a one-line preview for the list, taken from the message text once it is downloaded.
    pub fn set_preview(&self, id: MessageId, text: &str) -> Result<(), MailError> {
        let preview: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
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
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok(Hit { message: summary(r)?, folder: r.get(10)? })).map_err(db)?;
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
    fn a_preview_is_one_clean_line() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "a", 1, false)]).unwrap();
        let id = store.id_of("me", "INBOX", 1).unwrap().unwrap();
        store.set_preview(id, "Hello\n\n  world").unwrap();
        assert_eq!(store.messages("me", "INBOX").unwrap()[0].preview, "Hello world");
        assert!(store.search("me", "world", 10).unwrap().len() == 1, "the preview is searchable");
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
        let known = store.correspondents("me").unwrap();
        assert_eq!(known.len(), 2);
        assert!(known.contains("bob@x.org") && known.contains("carol@y.org") && !known.contains("me@z.org"));
        // A row from before recipients were kept is read later, then counts.
        store.conn().execute("UPDATE messages SET recipients = NULL WHERE folder = 'Sent' AND uid = 2", []).unwrap();
        assert_eq!(store.unknown_recipients("me", "Sent", 10).unwrap(), [2]);
        store.set_recipients("me", "Sent", 2, "dan@x.org").unwrap();
        assert!(store.correspondents("me").unwrap().contains("dan@x.org"));
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
        let replies = store.replies_to_sent("me", "INBOX").unwrap();
        assert_eq!(replies.len(), 2);
        assert!(!replies.contains(&id));
        store.conn().execute("UPDATE messages SET headers = NULL WHERE id = ?1", [id.0 as i64]).unwrap();
        assert_eq!((store.headers(id).unwrap(), store.unknown_headers("me", "INBOX", 10).unwrap()), (None, vec![1]));
        store.set_headers("me", "INBOX", 1, b"From: a@b").unwrap();
        assert_eq!(store.headers(id).unwrap().as_deref(), Some(&b"From: a@b"[..]));
    }
}
