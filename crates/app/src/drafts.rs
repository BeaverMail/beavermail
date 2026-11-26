//! Drafts kept on this computer, in the vault (so they are encrypted like downloaded mail).
//!
//! `drafts/<account>/<id>.json` holds the text; the attachments are copied next to it under
//! `drafts/<account>/<id>/`, so a draft keeps working when the original files move or the message it
//! forwards disappears.

use std::path::{Path, PathBuf};

use beavermail_core::MailError;
use beavermail_vault::{Vault, component};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftData {
    pub id: String,
    pub title: String,
    pub to: String,
    pub cc: String,
    #[serde(default)]
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub files: Vec<DraftFile>,
    /// Seconds since the Unix epoch.
    pub updated: i64,
    /// In the Outbox: Send was pressed without a connection; it goes when the server can be reached.
    #[serde(default)]
    pub queued: bool,
    /// With `queued`: not before this moment (seconds since the Unix epoch), "send later".
    #[serde(default)]
    pub send_at: Option<i64>,
    /// The user chose to send it encrypted (`Some(true)`) or not; `None`: encrypted when it can be.
    #[serde(default)]
    pub encrypt: Option<bool>,
    /// An answer to an invitation: the iTIP part it carries.
    #[serde(default)]
    pub calendar: Option<String>,
    /// The server's Drafts folder holds this text: from then on the draft is the server's, and kept
    /// here only to go on writing it.
    #[serde(default)]
    pub on_server: bool,
    /// The alias it is sent from; empty for the account's own address.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alias: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftFile {
    pub name: String,
    pub size: u64,
    /// Where the copy is in the vault.
    pub path: PathBuf,
}

/// An id no other draft has: the time in nanoseconds plus a counter.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn folder(account: &str) -> PathBuf {
    Path::new("drafts").join(component(account))
}

fn json_path(account: &str, id: &str) -> PathBuf {
    folder(account).join(format!("{}.json", component(id)))
}

/// Where a draft's attachments live.
pub fn files_dir(account: &str, id: &str) -> PathBuf {
    folder(account).join(component(id))
}

pub fn save(vault: &Vault, account: &str, draft: &DraftData) -> Result<(), MailError> {
    let json = serde_json::to_vec(draft).map_err(|e| MailError::Backend(format!("draft: {e}")))?;
    vault.write(&json_path(account, &draft.id), &json)
}

/// Draft `id`, when it is kept here.
pub fn load(vault: &Vault, account: &str, id: &str) -> Option<DraftData> {
    vault.read(&json_path(account, id)).ok().flatten().and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Where the ids of drafts deleted here, whose copy is still to be removed from the server, are kept.
fn deleted_path(account: &str) -> PathBuf {
    Path::new("drafts").join(format!("{}.deleted", component(account)))
}

/// The drafts deleted here whose copy on the server is still to be removed.
pub fn deleted(vault: &Vault, account: &str) -> Vec<String> {
    vault.read(&deleted_path(account)).ok().flatten().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Notes (`gone`) or forgets that draft `id`'s copy on the server is still to be removed.
pub fn set_deleted(vault: &Vault, account: &str, id: &str, gone: bool) -> Result<(), MailError> {
    let mut ids = deleted(vault, account);
    ids.retain(|i| i != id);
    if gone {
        ids.push(id.to_owned());
    }
    let json = serde_json::to_vec(&ids).map_err(|e| MailError::Backend(format!("draft: {e}")))?;
    vault.write(&deleted_path(account), &json)
}

/// What to do with a draft kept here, not queued to send, once the server says whether its Drafts
/// folder has a copy (`there`).
#[derive(Debug, PartialEq, Eq)]
pub enum Reconcile {
    /// Its text is not on the server yet (written offline, or a copy that failed): store it there. It
    /// goes even when a copy is there, which it replaces: changes made here are never lost.
    Upload,
    /// It was on the server and is gone: sent or deleted elsewhere, so it goes here too.
    Forget,
    Keep,
}

pub fn reconcile(draft: &DraftData, there: bool) -> Reconcile {
    match (draft.on_server, there) {
        (false, _) => Reconcile::Upload,
        (true, false) => Reconcile::Forget,
        (true, true) => Reconcile::Keep,
    }
}

/// The account's drafts, the most recently changed first. One that cannot be read is skipped.
pub fn load_all(vault: &Vault, account: &str) -> Vec<DraftData> {
    let mut drafts: Vec<DraftData> = vault
        .list(&folder(account))
        .unwrap_or_default()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| vault.read(p).ok().flatten())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    drafts.sort_by_key(|d| std::cmp::Reverse(d.updated));
    drafts
}

/// Deletes a draft and the copies of its attachments.
pub fn delete(vault: &Vault, account: &str, id: &str) -> Result<(), MailError> {
    vault.remove(&json_path(account, id))?;
    vault.remove_dir(&files_dir(account, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(id: &str, subject: &str, updated: i64) -> DraftData {
        DraftData { id: id.into(), title: "New message".into(), subject: subject.into(), updated, ..DraftData::default() }
    }

    #[test]
    fn drafts_round_trip_newest_first_and_stay_per_account() {
        let vault = Vault::in_memory();
        save(&vault, "me@example.org", &draft("a", "older", 10)).unwrap();
        save(&vault, "me@example.org", &draft("b", "newer", 20)).unwrap();
        save(&vault, "you@example.org", &draft("c", "someone else's", 30)).unwrap();
        let mine = load_all(&vault, "me@example.org");
        assert_eq!(mine.iter().map(|d| d.subject.as_str()).collect::<Vec<_>>(), ["newer", "older"]);
        assert_eq!(load_all(&vault, "you@example.org").len(), 1);
        // Saving again replaces, it does not add.
        save(&vault, "me@example.org", &draft("a", "older, edited", 40)).unwrap();
        assert_eq!(load_all(&vault, "me@example.org")[0].subject, "older, edited");
        assert_eq!(load_all(&vault, "me@example.org").len(), 2);
    }

    #[test]
    fn a_draft_follows_the_server_once_it_is_there() {
        let written_here = draft("a", "x", 1);
        let uploaded = DraftData { on_server: true, ..draft("a", "x", 1) };
        assert_eq!(reconcile(&written_here, false), Reconcile::Upload);
        // Changed here after its copy went up: the change goes up too, even if the copy was sent elsewhere.
        assert_eq!(reconcile(&written_here, true), Reconcile::Upload);
        assert_eq!(reconcile(&uploaded, true), Reconcile::Keep);
        assert_eq!(reconcile(&uploaded, false), Reconcile::Forget);
    }

    #[test]
    fn deleted_drafts_wait_until_the_server_lets_them_go() {
        let vault = Vault::in_memory();
        set_deleted(&vault, "me", "a", true).unwrap();
        set_deleted(&vault, "me", "b", true).unwrap();
        set_deleted(&vault, "me", "a", true).unwrap();
        assert_eq!(deleted(&vault, "me"), ["b", "a"]);
        set_deleted(&vault, "me", "b", false).unwrap();
        assert_eq!(deleted(&vault, "me"), ["a"]);
        // Not read as a draft of the account.
        assert!(load_all(&vault, "me").is_empty());
    }

    #[test]
    fn deleting_a_draft_removes_its_attachments_too() {
        let vault = Vault::in_memory();
        save(&vault, "me", &draft("a", "x", 1)).unwrap();
        let copy = files_dir("me", "a").join("0-plan.pdf");
        vault.write(&copy, b"data").unwrap();
        delete(&vault, "me", "a").unwrap();
        assert!(load_all(&vault, "me").is_empty());
        assert_eq!(vault.read(&copy).unwrap(), None);
    }

    #[test]
    fn a_draft_is_found_until_it_is_deleted() {
        let vault = Vault::in_memory();
        assert_eq!(load(&vault, "me", "a"), None);
        save(&vault, "me", &draft("a", "x", 1)).unwrap();
        assert_eq!(load(&vault, "me", "a").map(|d| d.subject), Some("x".to_owned()));
        delete(&vault, "me", "a").unwrap();
        assert_eq!(load(&vault, "me", "a"), None);
    }

    #[test]
    fn ids_are_not_repeated() {
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
    }
}
