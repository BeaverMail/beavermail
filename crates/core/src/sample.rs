// SPDX-License-Identifier: AGPL-3.0-only

//! An in-memory [`MailSource`] with generated data, for development and tests.

use crate::{MailSource, Mailbox, MessageId, MessageSummary};

const FOLDERS: [&str; 6] = ["Inbox", "Starred", "Drafts", "Sent", "Archive", "Trash"];
const SENDERS: [&str; 8] = [
    "Ada Lovelace", "Grace Hopper", "Linus Torvalds", "Marie Curie",
    "Alan Turing", "Radia Perlman", "Ken Thompson", "Barbara Liskov",
];
const WORDS: [&str; 12] = [
    "invoice", "meeting", "review", "draft", "release", "budget",
    "travel", "schedule", "update", "proposal", "notes", "server",
];

/// Generated messages, deterministic for a given count.
pub struct SampleSource {
    messages: Vec<(String, MessageSummary, String)>,
}

impl SampleSource {
    pub fn new(count: usize) -> Self {
        let mut seed: u64 = 0x5eed_0000_0000_0001;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize
        };
        let messages = (0..count)
            .map(|i| {
                let folder = FOLDERS[next() % 3].to_owned(); // mostly Inbox, Starred, Drafts
                let subject = format!("{} {}", WORDS[next() % WORDS.len()], WORDS[next() % WORDS.len()]);
                let body = (0..40).map(|_| WORDS[next() % WORDS.len()]).collect::<Vec<_>>().join(" ");
                let summary = MessageSummary {
                    id: MessageId(i as u64),
                    from: SENDERS[next() % SENDERS.len()].to_owned(),
                    subject,
                    preview: body.chars().take(90).collect(),
                    received: format!("{:02}:{:02}", next() % 24, next() % 60),
                    unread: next() % 3 == 0,
                };
                (folder, summary, body)
            })
            .collect();
        Self { messages }
    }
}

impl MailSource for SampleSource {
    fn mailboxes(&self) -> Vec<Mailbox> {
        FOLDERS
            .iter()
            .map(|name| Mailbox {
                name: (*name).to_owned(),
                unread: self
                    .messages
                    .iter()
                    .filter(|(folder, m, _)| folder == name && m.unread)
                    .count() as u32,
            })
            .collect()
    }

    fn messages(&self, mailbox: &str) -> Vec<MessageSummary> {
        self.messages
            .iter()
            .filter(|(folder, _, _)| folder == mailbox)
            .map(|(_, summary, _)| summary.clone())
            .collect()
    }

    fn body(&self, id: MessageId) -> Option<String> {
        self.messages.iter().find(|(_, m, _)| m.id == id).map(|(_, _, body)| body.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_count_gives_same_data() {
        let a = SampleSource::new(100);
        let b = SampleSource::new(100);
        assert_eq!(a.messages("Inbox"), b.messages("Inbox"));
    }

    #[test]
    fn every_message_has_a_body() {
        let source = SampleSource::new(50);
        for mailbox in source.mailboxes() {
            for m in source.messages(&mailbox.name) {
                assert!(source.body(m.id).is_some());
            }
        }
    }

    #[test]
    fn unread_counts_match_messages() {
        let source = SampleSource::new(200);
        for mailbox in source.mailboxes() {
            let unread = source.messages(&mailbox.name).iter().filter(|m| m.unread).count();
            assert_eq!(mailbox.unread as usize, unread);
        }
    }
}
