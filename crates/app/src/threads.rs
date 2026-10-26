// SPDX-License-Identifier: AGPL-3.0-only

//! Conversations: messages joined by the `In-Reply-To` and `References` headers. A message without
//! them joins by subject only when it says it is a reply or a forward ("Re:", "Fwd:"), so unrelated
//! mail that happens to share a subject (invoices, notifications) stays apart.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use beavermail_core::{MessageId, MessageSummary};

/// For each message, the number of its conversation: messages with the same number belong together.
/// Numbers run from 0 in the order conversations are first met.
pub fn group(messages: &[&MessageSummary]) -> Vec<usize> {
    let mut sets = Sets::default();
    // A message without a Message-ID still needs a node of its own.
    let own: Vec<usize> = messages
        .iter()
        .map(|m| if m.message_id.is_empty() { sets.fresh() } else { sets.node(&m.message_id) })
        .collect();
    for (m, &node) in messages.iter().zip(&own) {
        for parent in &m.parents {
            let parent = sets.node(parent);
            sets.join(node, parent);
        }
    }
    // A reply without headers joins the conversation of another message with the same subject: one
    // that is not such a reply itself if there is any, as the list is newest first.
    let loose = |m: &MessageSummary| m.parents.is_empty() && is_reply(&m.subject);
    let mut by_subject: HashMap<String, usize> = HashMap::new();
    for pass_loose in [false, true] {
        for (m, &node) in messages.iter().zip(&own).filter(|(m, _)| loose(m) == pass_loose) {
            by_subject.entry(subject_key(&m.subject)).or_insert(node);
        }
    }
    for (m, &node) in messages.iter().zip(&own).filter(|(m, _)| loose(m)) {
        let key = subject_key(&m.subject);
        if !key.is_empty() && let Some(&other) = by_subject.get(&key) {
            sets.join(node, other);
        }
    }
    let mut numbers: HashMap<usize, usize> = HashMap::new();
    own.iter()
        .map(|&node| {
            let root = sets.root(node);
            let next = numbers.len();
            *numbers.entry(root).or_insert(next)
        })
        .collect()
}

/// The conversations of the folder shown, with the user's own replies from Sent.
#[derive(Default)]
pub struct Conversations {
    /// What they were worked out from: the ids and unread flags of both lists.
    key: u64,
    /// The conversation of each message.
    of: HashMap<MessageId, usize>,
    /// Each conversation's messages, oldest first.
    members: Vec<Vec<MessageId>>,
    /// Whether a conversation has unread mail in the folder (Sent does not count).
    unread: Vec<bool>,
}

impl Conversations {
    /// Works them out again when `folder` or `sent` changed since last time.
    pub fn update(&mut self, folder: &[MessageSummary], sent: &[MessageSummary]) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for m in folder.iter().chain(sent) {
            (m.id, m.unread).hash(&mut hasher);
        }
        folder.len().hash(&mut hasher);
        let key = hasher.finish();
        if key == self.key && !self.of.is_empty() {
            return;
        }
        // The folder first: a message in both lists (the Sent folder shown) counts once, as the folder's.
        let mut seen = std::collections::HashSet::new();
        let all: Vec<(&MessageSummary, bool)> = folder.iter().map(|m| (m, true)).chain(sent.iter().map(|m| (m, false))).filter(|(m, _)| seen.insert(m.id)).collect();
        let numbers = group(&all.iter().map(|(m, _)| *m).collect::<Vec<_>>());
        let count = numbers.iter().max().map_or(0, |n| n + 1);
        let mut members: Vec<Vec<(i64, MessageId)>> = vec![Vec::new(); count];
        let mut unread = vec![false; count];
        for ((m, in_folder), &n) in all.iter().zip(&numbers) {
            members[n].push((m.timestamp, m.id));
            unread[n] |= *in_folder && m.unread;
        }
        *self = Self {
            key,
            of: all.iter().zip(&numbers).map(|((m, _), &n)| (m.id, n)).collect(),
            members: members
                .into_iter()
                .map(|mut list| {
                    list.sort_by_key(|(time, id)| (*time, id.0));
                    list.into_iter().map(|(_, id)| id).collect()
                })
                .collect(),
            unread,
        };
    }

    /// The messages of `id`'s conversation, oldest first; empty when it is in none.
    pub fn members(&self, id: MessageId) -> &[MessageId] {
        self.of.get(&id).map_or(&[], |&n| self.members[n].as_slice())
    }

    pub fn same(&self, a: MessageId, b: MessageId) -> bool {
        a == b || self.of.get(&a).is_some_and(|n| self.of.get(&b) == Some(n))
    }

    /// The conversation has unread mail in the folder.
    pub fn unread(&self, id: MessageId) -> bool {
        self.of.get(&id).is_some_and(|&n| self.unread[n])
    }

    pub fn number(&self, id: MessageId) -> Option<usize> {
        self.of.get(&id).copied()
    }
}

/// The prefixes mail programs put before a subject when answering or passing a message on.
const PREFIXES: [&str; 11] = ["re", "fw", "fwd", "aw", "wg", "sv", "vs", "r", "rif", "tr", "i"];

/// The subject without its "Re:"/"Fwd:" prefixes (repeated, any case, "Re[2]:" too), in lower case.
pub fn subject_key(subject: &str) -> String {
    let mut rest = subject.trim();
    while let Some(after) = strip_prefix(rest) {
        rest = after.trim_start();
    }
    rest.to_lowercase()
}

fn is_reply(subject: &str) -> bool {
    strip_prefix(subject.trim()).is_some()
}

fn strip_prefix(subject: &str) -> Option<&str> {
    let (word, rest) = subject.split_once(':')?;
    let word = word.trim_end();
    let word = word.split_once('[').map_or(word, |(w, n)| if n.trim_end_matches(']').chars().all(|c| c.is_ascii_digit()) { w } else { "" });
    PREFIXES.iter().any(|p| word.eq_ignore_ascii_case(p)).then_some(rest)
}

/// Disjoint sets over Message-IDs (union-find).
#[derive(Default)]
struct Sets {
    parent: Vec<usize>,
    ids: HashMap<String, usize>,
}

impl Sets {
    fn fresh(&mut self) -> usize {
        self.parent.push(self.parent.len());
        self.parent.len() - 1
    }

    fn node(&mut self, id: &str) -> usize {
        if let Some(&n) = self.ids.get(id) {
            return n;
        }
        let n = self.fresh();
        self.ids.insert(id.to_owned(), n);
        n
    }

    fn root(&mut self, mut n: usize) -> usize {
        while self.parent[n] != n {
            self.parent[n] = self.parent[self.parent[n]];
            n = self.parent[n];
        }
        n
    }

    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.root(a), self.root(b));
        if a != b {
            self.parent[b] = a;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u64, message_id: &str, parents: &[&str], subject: &str) -> MessageSummary {
        MessageSummary {
            id: MessageId(id),
            from: String::new(),
            address: String::new(),
            subject: subject.into(),
            preview: String::new(),
            received: String::new(),
            timestamp: 0,
            unread: false,
            pinned: false,
            starred: false,
            priority: 0,
            labels: Vec::new(),
            has_attachments: false,
            message_id: message_id.into(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn groups(list: &[MessageSummary]) -> Vec<usize> {
        group(&list.iter().collect::<Vec<_>>())
    }

    #[test]
    fn replies_join_by_their_headers_even_without_the_message_they_answer() {
        let list = [
            message(1, "c@x", &["a@x", "b@x"], "Re: Plan"),
            message(2, "z@x", &[], "Other"),
            message(3, "b@x", &["a@x"], "Re: Plan"),
            message(4, "d@x", &["a@x"], "Re: Plan"), // a@x itself is not here
        ];
        assert_eq!(groups(&list), [0, 1, 0, 0]);
    }

    #[test]
    fn a_reply_without_headers_joins_by_subject_but_plain_mail_never_does() {
        let list = [
            message(2, "b@x", &[], "RE: AW: Invoice 42"), // newest first, as listed
            message(1, "a@x", &[], "Invoice 42"),
            message(3, "c@x", &[], "Invoice 42"),
            message(4, "", &[], "Re[2]: invoice 42"),
        ];
        let g = groups(&list);
        assert_eq!(g[0], g[1], "a reply finds its subject, listed before it");
        assert_eq!(g[3], g[0], "Re[2]: and a missing Message-ID too");
        assert_ne!(g[2], g[1], "the same subject without Re: is another message");
    }

    #[test]
    fn a_conversation_lists_the_folder_and_sent_oldest_first() {
        let mut inbox = vec![message(3, "c@x", &["a@x"], "Re: Plan"), message(1, "a@x", &[], "Plan"), message(9, "z@x", &[], "Other")];
        inbox[0].timestamp = 30;
        inbox[1].timestamp = 10;
        inbox[0].unread = true;
        let mut sent = vec![message(2, "b@x", &["a@x"], "Re: Plan")];
        sent[0].timestamp = 20;
        sent[0].unread = true; // Sent never makes a conversation unread
        let mut c = Conversations::default();
        c.update(&inbox, &sent);
        assert_eq!(c.members(MessageId(1)), [MessageId(1), MessageId(2), MessageId(3)]);
        assert!(c.same(MessageId(2), MessageId(3)) && !c.same(MessageId(9), MessageId(1)));
        assert!(c.unread(MessageId(1)) && !c.unread(MessageId(9)));
        inbox[0].unread = false;
        c.update(&inbox, &sent);
        assert!(!c.unread(MessageId(1)), "a change of flag is seen");
    }

    #[test]
    fn subjects_lose_only_real_prefixes() {
        assert_eq!(subject_key("Re: Fwd: Lunch"), "lunch");
        assert_eq!(subject_key("Ricevuta: 12"), "ricevuta: 12");
        assert!(!is_reply("Meeting: today"));
    }
}
