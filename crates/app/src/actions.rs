// SPDX-License-Identifier: AGPL-3.0-only

//! What can be done to a message (read, pin, star, label, archive, trash, spam), as shown at once and
//! made on the server: at once, after the wait for Undo, or once the server answers again.

use std::sync::Arc;

use beavermail_core::{MailError, MailSource, MessageId, MessageSummary, Role};
use beavermail_imap::ImapSource;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum Action {
    Unread(bool),
    /// Kept at the top of its folder (`true`), or not any more.
    Pin(bool),
    /// The star every client shows, on or off.
    Star(bool),
    /// The standard label `$label<n>`, on or off.
    Label(u8, bool),
    Archive,
    Trash,
    /// To the server's Spam folder.
    Spam,
    /// From the Spam folder back to the Inbox.
    NotSpam,
    /// To another folder of the same account, by name.
    Move(String),
    Erase,
    Reply,
    ReplyAll,
    Forward,
}

impl Action {
    /// Whether the message leaves the folder being shown.
    pub(crate) fn removes(&self) -> bool {
        matches!(self, Self::Archive | Self::Trash | Self::Spam | Self::NotSpam | Self::Move(_) | Self::Erase)
    }
}

/// Where the changes waiting for the server are kept, in the vault.
pub(crate) const OFFLINE_CHANGES: &str = "offline/changes.json";

/// How long an Archive or Move to Trash waits before going to the server, so Undo can take it back.
pub(crate) const UNDO_SECS: u64 = 10;

/// Archive or Move to Trash, shown at once but sent to the server only after [`UNDO_SECS`].
pub(crate) struct Pending {
    /// For the bar: "Archived 3 messages".
    pub(crate) text: String,
    pub(crate) until: std::time::Instant,
    /// Each message: (account, folder index shown, the action, the message as listed, its place in the list).
    pub(crate) items: Vec<(u64, usize, Action, MessageSummary, usize)>,
}

/// Runs `action` on message `id` on the server.
pub(crate) async fn perform(source: Arc<ImapSource>, id: MessageId, action: Action) -> Result<(), MailError> {
    match action {
        Action::Unread(unread) => source.set_unread(id, unread).await,
        Action::Pin(on) => source.set_pinned(id, on).await,
        Action::Star(on) => source.set_starred(id, on).await,
        Action::Label(n, on) => source.set_label(id, &format!("$label{n}"), on).await,
        Action::Archive => source.archive(id).await,
        Action::Trash => source.trash(id).await,
        Action::Spam => source.move_to(id, Role::Junk).await,
        Action::NotSpam => source.move_to(id, Role::Inbox).await,
        Action::Move(folder) => source.move_to_folder(id, &folder).await,
        Action::Erase => source.erase(id).await,
        Action::Reply | Action::ReplyAll | Action::Forward => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_waits_offline_with_its_folder_and_older_queues_still_read() {
        let queued = vec![("alice@example.com".to_owned(), 5_u64, Action::Move("Work/Receipts".into()))];
        let json = serde_json::to_vec(&queued).unwrap();
        let back: Vec<(String, u64, Action)> = serde_json::from_slice(&json).unwrap();
        assert!(matches!(&back[0].2, Action::Move(folder) if folder == "Work/Receipts"));
        assert!(back[0].2.removes());
        // Written before moving to any folder was possible.
        let old: Vec<(String, u64, Action)> = serde_json::from_str(r#"[["alice@example.com",5,"Archive"],["alice@example.com",6,{"Unread":true}]]"#).unwrap();
        assert!(matches!(old[0].2, Action::Archive) && matches!(old[1].2, Action::Unread(true)));
    }
}
