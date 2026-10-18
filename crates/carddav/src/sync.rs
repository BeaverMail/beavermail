// SPDX-License-Identifier: AGPL-3.0-only

//! Keeping the stored cards of an address book and the server's the same.

use std::collections::{HashMap, HashSet};

use beavermail_core::MailError;
use beavermail_store::{CardFields, CardState, Store};

use crate::{Book, Written, vcard};

/// What one sync did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synced {
    /// Cards new or changed on the server, now stored.
    pub received: usize,
    /// Changes made here, now on the server.
    pub sent: usize,
    /// Changes made here that lost to a newer one on the server.
    pub lost: usize,
}

/// What a card shows, from its vCard.
pub fn fields(text: &str) -> CardFields {
    let card = vcard::parse(text);
    CardFields { name: card.name, emails: card.emails, phones: card.phones, note: card.note }
}

/// Sends what changed here, then brings what the server has. A card changed on both sides keeps the
/// server's version: the address book edited elsewhere is the fuller one, and nothing there is lost.
pub fn sync(book: &Book, store: &Store) -> Result<Synced, MailError> {
    let key = book.url.as_str();
    let mut done = Synced::default();
    for card in store.cards(key)? {
        let written = match card.state {
            CardState::Synced => continue,
            CardState::New => book.put(&card.href, &card.vcard, None)?,
            CardState::Changed => book.put(&card.href, &card.vcard, Some(&card.etag))?,
            CardState::Deleted => book.delete(&card.href, &card.etag)?,
        };
        match (written, card.state) {
            (Written::Done(_), CardState::Deleted) => store.forget_card(key, &card.href)?,
            // Without the new version from the server, it is read again below.
            (Written::Done(etag), _) => store.card_synced(card.id, &etag.unwrap_or_default())?,
            // Lost to the server's version, which is read again below.
            (Written::Conflict, _) => {
                store.card_synced(card.id, "")?;
                done.lost += 1;
                continue;
            }
        }
        done.sent += 1;
    }
    let remote = book.list()?;
    let stored: HashMap<String, String> = store.cards(key)?.into_iter().map(|c| (c.href, c.etag)).collect();
    let wanted: Vec<String> = remote.iter().filter(|r| stored.get(&r.href) != Some(&r.etag)).map(|r| r.href.clone()).collect();
    for (card, text) in book.fetch(&wanted)? {
        store.put_card(key, &card.href, &card.etag, &text, &fields(&text))?;
        done.received += 1;
    }
    // Deleted on the server: gone here too.
    let there: HashSet<&str> = remote.iter().map(|r| r.href.as_str()).collect();
    for card in store.cards(key)?.into_iter().filter(|c| c.state == CardState::Synced && !there.contains(c.href.as_str())) {
        store.forget_card(key, &card.href)?;
    }
    Ok(done)
}
