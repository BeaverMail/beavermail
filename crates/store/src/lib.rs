// SPDX-License-Identifier: AGPL-3.0-only

//! The local mail archive: folders, message headers, bodies and a full-text index in SQLite.
//!
//! Everything the UI shows comes from here first, so the window opens instantly and works
//! offline; the protocol backends only keep it up to date. Rows are keyed by account, folder and
//! IMAP UID, and a [`MessageId`] is the row id, which stays valid across sessions.
//!
//! shortcut: the database file is not encrypted yet; PRD 7.3 asks for a key held in the system
//! keychain (SQLCipher or file-level encryption), to be added before any release.

use std::path::Path;
use std::sync::Mutex;

use beavermail_core::{MailError, MessageId, MessageSummary, Role};
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
    conn: Mutex<Connection>,
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

fn summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: MessageId(row.get::<_, i64>(0)? as u64),
        from: row.get(1)?,
        address: row.get(2)?,
        subject: row.get(3)?,
        preview: row.get(4)?,
        received: row.get(5)?,
        timestamp: row.get(6)?,
        unread: row.get::<_, i64>(7)? != 0,
        has_attachments: row.get::<_, i64>(8)? != 0,
        message_id: row.get(9)?,
    })
}

const SUMMARY_COLUMNS: &str = "id, CASE WHEN from_name = '' THEN from_addr ELSE from_name END, from_addr, subject, preview, received, timestamp, unread, attach, message_id";

impl Store {
    /// Opens (and creates) the archive at `path`.
    pub fn open(path: &Path) -> Result<Self, MailError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(db)?;
        }
        Self::with(Connection::open(path).map_err(db)?)
    }

    /// A throwaway archive in memory, for tests.
    pub fn in_memory() -> Result<Self, MailError> {
        Self::with(Connection::open_in_memory().map_err(db)?)
    }

    fn with(conn: Connection) -> Result<Self, MailError> {
        conn.execute_batch(SCHEMA).map_err(db)?;
        // Version 2: message bodies moved out of the database into encrypted files (PRD D11).
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
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while the lock was held leaves consistent data (every write is a transaction).
        self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
                     (account, folder, uid, from_name, from_addr, subject, received, timestamp, unread, message_id, in_reply_to, refs, size, attach, attach_known)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1)",
                )
                .map_err(db)?;
            for h in headers {
                stmt.execute(params![
                    account, folder, i64::from(h.uid), h.from_name, h.from_addr, h.subject, h.received, h.timestamp, i64::from(h.unread),
                    h.message_id, h.in_reply_to, h.references, i64::from(h.size), i64::from(h.has_attachments)
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

    /// Messages of one account whose sender, subject or preview contain every word of `query`.
    pub fn search(&self, account: &str, query: &str, limit: usize) -> Result<Vec<MessageSummary>, MailError> {
        // Each word becomes a quoted prefix term, so user input can never be FTS syntax.
        let terms: Vec<String> = query.split_whitespace().map(|w| format!("\"{}\"*", w.replace('"', ""))).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages WHERE account = ?1 AND id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?2)
             ORDER BY timestamp DESC LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql).map_err(db)?;
        let rows = stmt.query_map(params![account, terms.join(" "), limit as i64], summary).map_err(db)?;
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
        let store = Store::with(conn).unwrap();
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
        let store = Store::with(conn).unwrap();
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
        // PRD section 5: search over 50k messages in 500 ms. Generous bounds so a slow CI box does not flake.
        assert!(search < std::time::Duration::from_millis(500), "search took {search:?}");
        assert!(list < std::time::Duration::from_secs(2), "list took {list:?}");
    }

    #[test]
    fn other_accounts_do_not_see_each_others_mail() {
        let store = store();
        store.add_headers("me", "INBOX", &[header(1, "mine", 1, false)]).unwrap();
        assert!(store.messages("you", "INBOX").unwrap().is_empty());
        assert!(store.search("you", "mine", 10).unwrap().is_empty());
    }
}
