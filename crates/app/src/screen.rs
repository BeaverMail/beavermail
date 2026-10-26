// SPDX-License-Identifier: AGPL-3.0-only

//! A second opinion on incoming mail from what only this computer knows: whom the user writes to, and
//! what the message contains. The server's filter judges each message alone; here a first message from
//! a stranger, a name borrowed from a colleague or a domain one letter off a familiar one stand out.
//!
//! Mail from people the user knows is never touched. Mail from a new sender reaches the Inbox unless a
//! check fails; then it is held apart as suspicious, and the user can mark it, or its sender, otherwise.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use beavermail_accounts as accounts;
use beavermail_core::{Account, MailError, MailSource, MessageBody, MessageId, MessageSummary, Role, Settings};
use beavermail_imap::ImapSource;
use beavermail_store::Store;
use beavermail_vault::{SystemStore, Vault};
use serde::{Deserialize, Serialize};

use crate::{html, learn, sender};

/// Where a message goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Place {
    Inbox,
    /// Held apart from the Inbox as likely spam (on this computer only).
    Spam,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Level {
    /// Worth knowing, common in legitimate mail too.
    Notice,
    /// A sign of fraud or of something that can harm the computer.
    Danger,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

/// What this computer knows about the people an account deals with.
#[derive(Default)]
pub struct Known {
    /// Addresses the user wrote to, their own and the senders marked as not spam.
    addresses: HashSet<String>,
    /// Domains of the people the user writes to, and their own.
    domains: HashSet<String>,
    /// The organisations among them (registered domains, public mailbox providers left out): their
    /// other addresses are known too.
    organisations: HashSet<String>,
    /// A display name (normalised) and the known addresses that used it.
    names: HashMap<String, Vec<String>>,
    /// The names the user sends under (normalised), from their own messages.
    own_names: HashSet<String>,
    own: HashSet<String>,
    /// Inbox messages that answer one the user sent.
    replies: HashSet<MessageId>,
    /// Fingerprints of spam the user already has (the Spam folder, senders marked as spam), with the
    /// domain each came from.
    spam_prints: Vec<(Print, String)>,
    /// The classifier learnt from the user's mail; `None` until there are enough examples.
    model: Option<learn::Model>,
    /// Examples the model had: (spam, not spam).
    examples: (usize, usize),
}

impl Known {
    /// `own` is every address the user sends from, on any account.
    pub fn load(store: &Store, account: &Account, own: &[String], settings: &Settings) -> Result<Self, MailError> {
        let key = account.key();
        let mut known = Self::new(store.correspondents()?, own, &store.senders(&key)?, settings);
        // What the user has already judged: messages they called spam or not spam (kept, so they count
        // after moving or after the server empties its Spam folder), the Spam folder, mail from senders
        // marked as spam, and the mail of known senders, which reaches the Inbox whatever its content.
        // Each message counts once, as the user's own word first.
        let (mut spam, mut ham) = (Vec::new(), Vec::new());
        let mut seen = HashSet::new();
        for (id, is_spam, text, domain) in store.examples(&key)? {
            seen.insert(id);
            if is_spam { &mut spam } else { &mut ham }.push((text, domain));
        }
        let fresh = |m: &MessageSummary| m.message_id.is_empty() || !seen.contains(&m.message_id);
        if let Some(inbox) = store.folder_with_role(&key, Role::Inbox)? {
            known.replies = store.replies_to_sent(&key, &inbox)?;
            for m in store.messages(&key, &inbox)?.iter().filter(|m| fresh(m)) {
                match place(&known, settings, m.id, &m.address, false) {
                    (Place::Spam, ..) => spam.push(example(m)),
                    (Place::Inbox, _, false) => ham.push(example(m)),
                    (Place::Inbox, _, true) => {}
                }
            }
        }
        if let Some(junk) = store.folder_with_role(&key, Role::Junk)? {
            spam.extend(store.messages(&key, &junk)?.iter().filter(|m| fresh(m)).map(example));
        }
        known.spam_prints = spam.iter().filter_map(|(text, domain)| Some((fingerprint(text)?, domain.clone()))).collect();
        known.examples = (spam.len(), ham.len());
        known.model = learn::Model::train(&spam, &ham);
        Ok(known)
    }

    fn new(correspondents: HashSet<String>, own: &[String], senders: &[(String, String)], settings: &Settings) -> Self {
        let own: HashSet<String> = own.iter().map(|a| a.trim().to_lowercase()).collect();
        let own_names = senders.iter().filter(|(_, a)| own.contains(a)).filter_map(|(n, _)| normal_name(n)).collect();
        let mut addresses = correspondents;
        addresses.extend(own.iter().map(|a| a.trim().to_lowercase()));
        let domains: HashSet<String> = addresses.iter().map(|a| sender::domain(a)).filter(|d| !d.is_empty()).collect();
        let organisations = domains.iter().filter(|d| !sender::public_mail(d)).map(|d| sender::registrable(d)).collect();
        addresses.extend(settings.marked.iter().filter(|(_, not_spam)| **not_spam).map(|(a, _)| a.clone()));
        let mut names: HashMap<String, Vec<String>> = HashMap::new();
        for (name, address) in senders {
            if let Some(name) = normal_name(name)
                && addresses.contains(address)
            {
                names.entry(name).or_default().push(address.clone());
            }
        }
        Self { addresses, domains, organisations, names, own_names, own, replies: HashSet::new(), spam_prints: Vec::new(), model: None, examples: (0, 0) }
    }

    pub fn count(&self) -> usize {
        self.addresses.len()
    }

    /// What the learnt model had to learn from, (spam, not spam), and whether it is active.
    pub fn model(&self) -> ((usize, usize), Option<usize>) {
        (self.examples, self.model.as_ref().map(learn::Model::size))
    }

    /// How many fingerprints of spam the user already has are known.
    pub fn spam_prints(&self) -> usize {
        self.spam_prints.len()
    }
}

/// Who a mark is about: the organisation (registered domain) of the address, or the address itself at
/// a public mailbox provider, where the domain says nothing about the sender.
pub fn sender_key(address: &str) -> String {
    let address = address.trim().to_lowercase();
    let domain = sender::domain(&address);
    if domain.is_empty() || sender::public_mail(&domain) { address } else { format!("@{}", sender::registrable(&domain)) }
}

/// Where a message from `address` goes before its content is looked at, why, and whether its sender is
/// new. `forged`: the account's server says the message does not come from the domain it claims.
pub fn place(known: &Known, settings: &Settings, id: MessageId, address: &str, forged: bool) -> (Place, String, bool) {
    let address = address.trim().to_lowercase();
    let key = sender_key(&address);
    let organisation = key.strip_prefix('@').filter(|_| !address.is_empty());
    let mark = settings.mark_of(&address).map(|m| (m, "this sender")).or_else(|| organisation.and_then(|_| settings.mark_of(&key)).map(|m| (m, "everyone at this domain")));
    match mark {
        Some((false, who)) => (Place::Spam, format!("you marked {who} as spam"), false),
        _ if forged => (Place::Spam, "your server says the sender is forged".into(), false),
        Some((true, who)) => (Place::Inbox, format!("you marked {who} as not spam"), false),
        None if known.addresses.contains(&address) => (Place::Inbox, "you have written to this sender".into(), false),
        None if known.replies.contains(&id) => (Place::Inbox, "a reply to a message you sent".into(), false),
        None => match organisation.filter(|o| known.organisations.contains(*o)) {
            Some(o) => (Place::Inbox, format!("you write to others at {o}"), false),
            None => (Place::Inbox, "you have never written to this sender".into(), true),
        },
    }
}

/// What the checks made of one Inbox message; kept in the archive, so each message is checked once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Screened {
    pub place: Place,
    pub why: String,
    /// Who a mark is about: see [`sender_key`].
    pub key: String,
    /// The sender is new: nothing the user did or wrote vouches for them.
    pub new_sender: bool,
    /// The checks' findings, from the whole message when it is stored, else from its header fields.
    pub findings: Vec<Finding>,
    /// The server's verdict on the sender; `None` when nothing of the message is stored to read it from.
    pub verdict: Option<sender::Authenticity>,
    /// The whole message was checked, not only its header fields.
    pub whole: bool,
}

impl Screened {
    /// Applies the user's mark on this sender: not spam (unless the server says the sender is forged) or spam.
    pub fn mark(&mut self, not_spam: bool) {
        let forged = self.verdict == Some(sender::Authenticity::Failed);
        self.new_sender = false;
        self.place = if not_spam && !forged { Place::Inbox } else { Place::Spam };
    }
}

/// Checks `messages` of an Inbox (see [`screen_message`]), then holds mail from new senders whose text
/// is a campaign already held: spam you have, mail `held` before, or a message held in this same pass.
pub fn screen_all(source: &ImapSource, known: &Known, settings: &Settings, server_host: &str, messages: &[MessageSummary], held: &[MessageSummary]) -> Vec<Screened> {
    let mut all: Vec<Screened> = messages.iter().map(|m| screen_message(source, known, settings, server_host, m)).collect();
    let prints: Vec<Option<Print>> = messages.iter().map(|m| fingerprint(&text_of(m))).collect();
    // Domains as organisations: mail.x.com and x.com are one sender, not one campaign under two names.
    let organisation = |m: &MessageSummary| sender::registrable(&sender::domain(&m.address));
    let mut held: Vec<(Print, String)> = known.spam_prints.iter().cloned().chain(held.iter().filter_map(|m| Some((fingerprint(&text_of(m))?, organisation(m))))).collect();
    held.extend(all.iter().zip(&prints).zip(messages).filter(|((s, _), _)| s.place == Place::Spam).filter_map(|((_, p), m)| Some(((*p)?, organisation(m)))));
    for ((screened, print), m) in all.iter_mut().zip(&prints).zip(messages) {
        let domain = organisation(m);
        // The same text from the same domain is the same sender, which its own checks judge.
        let twin = print.and_then(|p| held.iter().find(|(h, d)| *d != domain && similarity(&p, h) >= SAME_CAMPAIGN));
        // shortcut: a reply quotes its thread, which matches any message of it held as spam, so replies
        // are not compared; a campaign that sets In-Reply-To passes this check (the others still apply).
        // Compare with the held message's thread instead once its Message-ID is kept with its print.
        if screened.new_sender && screened.place == Place::Inbox && m.parents.is_empty() && let Some((_, from)) = twin {
            let text = format!("Its text is the same as spam you already have, sent from {from}: one campaign under many names");
            screened.findings.insert(0, Finding { level: Level::Danger, text: text.clone() });
            screened.place = Place::Spam;
            screened.why = format!("new sender: {text}");
        }
        // Last, what the user's own spam taught: alone it holds nothing, as the text of a whole language
        // can read as spam; with a lesser finding, which holds nothing alone either, it holds the message.
        if let Some(model) = &known.model
            && screened.new_sender
            && screened.place == Place::Inbox
            && !screened.findings.is_empty()
        {
            let (spam, words) = model.score(&text_of(m), &domain);
            if spam >= HOLD_PROBABILITY {
                let text = format!("Its words are like those of the spam you have ({:.0}%: {})", spam * 100.0, words.join(", "));
                screened.findings.push(Finding { level: Level::Notice, text: text.clone() });
                screened.place = Place::Spam;
                screened.why = format!("new sender: {text}");
            }
        }
    }
    all
}

/// The learnt model joins a lesser finding to hold a new sender's mail from this probability of spam
/// up: it errs towards the Inbox, where a missed spam costs a glance and a held message could cost a reply.
const HOLD_PROBABILITY: f32 = 0.9;

/// A min-hash of a text's three-word phrases: texts that share most of
/// their phrases share most of the values, whatever was changed around them.
type Print = [u64; 32];

/// From this share of equal values up, two texts are taken for one campaign.
const SAME_CAMPAIGN: f32 = 0.6;

/// What a fingerprint is taken of: the subject and the start of the text, the parts the list shows.
fn text_of(m: &MessageSummary) -> String {
    format!("{} {}", m.subject, m.preview)
}

/// A message as a training example: its text and the organisation it came from.
pub fn example(m: &MessageSummary) -> (String, String) {
    (text_of(m), sender::registrable(&sender::domain(&m.address)))
}

/// `None` for a text too short to tell a campaign by.
fn fingerprint(text: &str) -> Option<Print> {
    use std::hash::{Hash, Hasher};
    let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(str::to_lowercase).collect();
    if words.len() < 6 {
        return None;
    }
    let mut print = [u64::MAX; 32];
    for phrase in words.windows(3) {
        for (seed, slot) in print.iter_mut().enumerate() {
            let mut hasher = std::hash::DefaultHasher::new();
            (seed, phrase).hash(&mut hasher);
            *slot = (*slot).min(hasher.finish());
        }
    }
    Some(print)
}

fn similarity(a: &Print, b: &Print) -> f32 {
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f32 / a.len() as f32
}

/// Checks message `m` of an Inbox from what is stored of it: the whole message, or its header fields.
/// A new sender whose mail fails a serious check is held as spam; a lesser one (a [`Level::Notice`])
/// holds it only with the learnt model's word (see [`screen_all`]); anyone the user knows never is.
pub fn screen_message(source: &ImapSource, known: &Known, settings: &Settings, server_host: &str, m: &MessageSummary) -> Screened {
    let (body, whole) = match source.stored_body(m.id) {
        Ok(Some(body)) => (Some(body), true),
        _ => (source.header_body(m.id).ok().flatten(), false),
    };
    let verdict = body.as_ref().map(|b| sender::authenticity(&b.auth, server_host, &m.address));
    let (mut place, mut why, new_sender) = place(known, settings, m.id, &m.address, verdict == Some(sender::Authenticity::Failed));
    let findings = body.map(|b| findings(known, &m.from, &m.address, &b, server_host)).unwrap_or_default();
    if new_sender && let Some(first) = findings.first().filter(|f| f.level == Level::Danger) {
        place = Place::Spam;
        why = format!("new sender: {}", first.text);
    }
    Screened { place, why, key: sender_key(&m.address), new_sender, findings, verdict, whole }
}

/// Everything about a message that deserves a second look, the most serious first.
pub fn findings(known: &Known, name: &str, address: &str, body: &MessageBody, server_host: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut add = |level, text: String| out.push(Finding { level, text });
    let address = address.trim().to_lowercase();
    let from = sender::domain(&address);
    let stranger = !known.addresses.contains(&address);

    let authenticity = sender::authenticity(&body.auth, server_host, &address);
    if authenticity == sender::Authenticity::Failed {
        add(Level::Danger, "Your mail server checked this message and it does not come from the domain it claims. It is probably forged".into());
    }
    // Your own name on someone else's address: the "I hacked your account" extortion, or a fake
    // invitation. It says all the name checks below would.
    let ours = known.own.iter().any(|o| *o == address || (sender_key(o).starts_with('@') && sender_key(o) == sender_key(&address)));
    let own_name = !ours && normal_name(name).is_some_and(|n| known.own_names.contains(&n));
    if own_name {
        add(Level::Danger, format!("It uses your own name, but comes from {address}"));
    }
    // Someone you write to keeps the name they chose ("Registro.it" writing from nic.it).
    if stranger && !own_name && let Some(why) = sender::warning(name, &address) {
        add(Level::Danger, format!("{why}: {address}"));
    }
    if body.spam {
        add(Level::Danger, "Your mail server's spam filter flagged this message".into());
    }
    // The spam filter's own report, read only to raise suspicion: a sender gains nothing by writing it.
    if body.spam_tests.iter().any(|t| t == "SPF_FAIL") {
        add(Level::Danger, format!("Your mail server found that {from} did not authorise the computer that sent this (SPF failed)"));
    }
    // A colleague's name on a stranger's address: the usual "it's me, I lost my phone" fraud. Another
    // address of the same organisation (sales@ and support@) is not one.
    if stranger
        && !own_name
        && let Some(others) = normal_name(name).and_then(|n| known.names.get(&n))
        && !others.iter().any(|o| sender_key(o) == sender_key(&address))
        && let Some(other) = others.iter().min()
    {
        add(Level::Danger, format!("{} is the name of someone you know at {other}, but this comes from {address}", name.trim()));
    }
    // "PayPal" from cheap-batteries.example: a brand in the name, a domain that is not the brand's.
    // An organisation you deal with often writes through someone else's domain (a service that sends
    // for it), so its name counts only from a mailbox anyone can open, or from a domain
    // that borrows a word of its own ("Acme Corp" from acme-invoices.net).
    let words = |label: String| label.split('-').filter(|w| w.len() >= 4).map(str::to_owned).collect::<Vec<_>>();
    let borrowed = |owns: &[String]| sender::public_mail(&from) || words(organisation_label(&from)).iter().any(|w| owns.iter().any(|o| words(o.clone()).contains(w)));
    // A name that is only the address ("billing.acme@agency.example") shows where it comes from.
    if stranger
        && !from.is_empty()
        && !sender::names_the_address(name, &address)
        && let Some(brand) = brand_in(name, known).filter(|(_, owns, ours)| !owns.contains(&organisation_label(&from)) && !(*ours && (PLATFORMS.contains(&organisation_label(&from).as_str()) || !borrowed(owns))))
    {
        add(Level::Danger, format!("The name says {}, but the message comes from {from}", brand.0));
    }
    if stranger && machine_made(address.split('@').next().unwrap_or("")) {
        add(Level::Notice, format!("The address {address} looks generated by a machine, a sign of bulk spam"));
    }
    if !from.is_empty()
        && !known.domains.iter().any(|k| sender::same_site(k, &from))
        && let Some(like) = known.domains.iter().filter(|k| looks_like(k, &from)).min()
    {
        add(Level::Danger, format!("The domain {from} looks like {like}, which you write to"));
    }
    // Replies diverted to a mailbox anyone can open, or a private mailbox sending them elsewhere. A
    // company answering from its own domain (a shop sent through Shopify) is normal.
    let diverted = |r: &String| {
        let to = sender::domain(r);
        !to.is_empty() && !sender::same_site(&to, &from) && (sender::public_mail(&to) || sender::public_mail(&from))
    };
    if let Some(reply) = body.reply_to.iter().map(|r| bare(r)).find(diverted) {
        add(Level::Notice, format!("Replies would go to {reply}, not to {from}"));
    }

    if let Some(markup) = &body.html {
        let links: Vec<(String, String)> = html::blocks(markup)
            .into_iter()
            .flat_map(|b| b.runs.into_iter().chain(b.rows.into_iter().flatten().flatten()))
            .filter_map(|r| match r {
                html::Run::Link { text, url } => Some((text, url)),
                html::Run::Text { .. } => None,
            })
            .collect();
        // A link to the sender's own domain is its click tracking, not a disguise; so is one that shows
        // the sender's own site when the server verified the sender (the tracking of the service that sends its mail).
        // By where each link really leads: through a known redirect, to its target.
        let own_site = |text: &str| authenticity == sender::Authenticity::Verified && sender::registrable(&html::host(text)) == sender::registrable(&from);
        if let Some((text, url)) = links.iter().find(|(text, url)| {
            let target = html::clean_url(url, "");
            html::looks_misleading(text, &target) && !sender::same_site(&html::host(&target), &from) && !own_site(text)
        }) {
            add(Level::Danger, format!("A link shows {} but opens {}", text.trim(), html::host(&html::clean_url(url, ""))));
        }
        if let Some(host) = links.iter().map(|(_, url)| html::host(url)).find(|h| h.parse::<std::net::IpAddr>().is_ok() || h.starts_with('[')) {
            add(Level::Danger, format!("A link opens {host}, a bare network address instead of a site"));
        }
        if let Some(host) = links.iter().map(|(_, url)| html::host(url)).find(|h| sender::disguised(h)) {
            add(Level::Danger, format!("A link opens {host}, written with look-alike characters"));
        }
        // A page uploaded to a storage or hosting service: free, anonymous, and on a reputable domain
        // that link filters let through. Shops and banks link to their own sites.
        if stranger && let Some(host) = links.iter().map(|(_, url)| html::host(url)).find(|h| hosted_page(h)) {
            add(Level::Danger, format!("A link opens a page uploaded to {host}, a common way to hide a phishing or scam page"));
        }
        let hidden = html::hidden_text(markup);
        if hidden >= HIDDEN_LIMIT {
            add(Level::Notice, format!("It hides {hidden} characters of text from you, a trick to get past spam filters"));
        }
    }

    for file in &body.attachments {
        let name = file.name.trim().trim_end_matches('.').to_lowercase();
        let mut parts = name.rsplit('.');
        let (ext, inner) = (parts.next().unwrap_or(""), parts.next().filter(|_| name.matches('.').count() >= 2).unwrap_or(""));
        if PROGRAMS.contains(&ext) {
            let disguise = if DOCUMENTS.contains(&inner) { format!(" disguised as a .{inner} file") } else { String::new() };
            add(Level::Danger, format!("{} is a program or script{disguise}: opening it can run code on this computer", file.name));
        } else if PAGES.contains(&ext) {
            add(Level::Danger, format!("{} is a web page: if it asks for a password, it is phishing", file.name));
        } else if MACROS.contains(&ext) {
            add(Level::Danger, format!("{} is an Office file with macros, which can run code", file.name));
        } else if ARCHIVES.contains(&ext) && stranger {
            add(Level::Notice, format!("{} is an archive from a sender you have never written to", file.name));
        }
    }

    out.sort_by_key(|f| std::cmp::Reverse(f.level));
    out
}

/// Brands that phishing borrows most, as a word of a display name, and the names of the domains they
/// really send from ("amazon" covers amazon.es and amazon.co.uk).
const BRANDS: [(&str, &[&str]); 52] = [
    ("paypal", &["paypal"]), ("amazon", &["amazon", "amazonaws"]), ("apple", &["apple", "icloud"]), ("icloud", &["apple", "icloud"]),
    ("microsoft", &["microsoft", "office", "outlook", "live", "microsoftonline", "office365"]), ("outlook", &["microsoft", "outlook", "office", "live"]),
    ("office365", &["microsoft", "office", "office365"]), ("google", &["google", "youtube", "gmail"]), ("netflix", &["netflix"]), ("spotify", &["spotify"]),
    ("facebook", &["facebook", "facebookmail", "meta"]), ("instagram", &["instagram", "facebookmail", "meta"]), ("whatsapp", &["whatsapp", "facebookmail", "meta"]),
    ("linkedin", &["linkedin"]), ("dhl", &["dhl"]), ("fedex", &["fedex"]), ("ups", &["ups"]), ("gls", &["gls-group", "gls-spain", "gls-italy", "gls"]), ("seur", &["seur"]),
    ("correos", &["correos"]), ("poste", &["poste", "posteitaliane"]), ("brt", &["brt"]), ("mercadona", &["mercadona"]), ("carrefour", &["carrefour"]),
    ("santander", &["santander", "bancosantander"]), ("bbva", &["bbva"]), ("caixabank", &["caixabank"]), ("sabadell", &["bancsabadell", "sabadell"]), ("bankinter", &["bankinter"]),
    ("ing", &["ing"]), ("unicredit", &["unicredit"]), ("intesa", &["intesasanpaolo"]), ("fineco", &["finecobank"]), ("n26", &["n26"]), ("revolut", &["revolut"]),
    ("binance", &["binance"]), ("coinbase", &["coinbase"]), ("docusign", &["docusign"]), ("dropbox", &["dropbox"]), ("adobe", &["adobe"]), ("ebay", &["ebay"]),
    ("booking", &["booking"]), ("airbnb", &["airbnb"]), ("garmin", &["garmin"]), ("vodafone", &["vodafone"]), ("movistar", &["movistar", "telefonica"]),
    ("endesa", &["endesa"]), ("iberdrola", &["iberdrola"]), ("enel", &["enel"]), ("aeat", &["aeat", "agenciatributaria"]), ("hacienda", &["aeat", "agenciatributaria"]),
    ("dgt", &["dgt"]),
];

/// The first brand `name` mentions: a word of [`BRANDS`], or an organisation the user writes to (as
/// one to three words: "Acme Corp" and "Acme-Corp" are acme-corp.com). Returns it as written, the
/// domain names that brand sends from, and whether it is one of the user's organisations.
fn brand_in(name: &str, known: &Known) -> Option<(String, Vec<String>, bool)> {
    let words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let listed = words.iter().find_map(|word| {
        let lower = word.to_lowercase();
        BRANDS.iter().find(|(b, _)| *b == lower).map(|(_, owns)| ((*word).to_owned(), owns.iter().map(|o| (*o).to_owned()).collect(), false))
    });
    // An organisation you deal with is a brand too: "Domainco" from another domain is not Domainco.
    let ours = || {
        (0..words.len()).flat_map(|i| (i + 1..=(i + 3).min(words.len())).map(move |j| (i, j))).find_map(|(i, j)| {
            let joined = words[i..j].concat().to_lowercase();
            let label = known.organisations.iter().map(|o| organisation_label(o)).find(|l| l.len() >= 4 && l.replace('-', "") == joined)?;
            Some((words[i..j].join(" "), vec![label], true))
        })
    };
    listed.or_else(ours)
}

/// A host that serves pages anyone can upload: cloud storage buckets and free static hosting.
fn hosted_page(host: &str) -> bool {
    const EXACT: [&str; 4] = ["storage.googleapis.com", "storage.cloud.google.com", "firebasestorage.googleapis.com", "ipfs.io"];
    const SUFFIXES: [&str; 10] = [".web.app", ".firebaseapp.com", ".blob.core.windows.net", ".web.core.windows.net", ".r2.dev", ".pages.dev", ".workers.dev", ".netlify.app", ".vercel.app", ".ipfs.dweb.link"];
    EXACT.contains(&host) || SUFFIXES.iter().any(|s| host.ends_with(s))
}

/// "amazon" for "mail.amazon.co.uk": the name an organisation registered, without its suffix.
fn organisation_label(domain: &str) -> String {
    sender::registrable(domain).split('.').next().unwrap_or("").to_owned()
}

/// A mailbox name nobody would choose: case flipping at random ("WqOAXUVzQcKN") or five consonants in a
/// row ("ewtkxdrhd"). Names like "support" or "noreply" have few vowels too, so vowels alone say nothing.
/// The tag after a `+` is routing a machine made on purpose (receipts+acct_1Pks@stripe.com) and is left out.
fn machine_made(local: &str) -> bool {
    let local = local.split('+').next().unwrap_or("");
    let letters: Vec<char> = local.chars().filter(char::is_ascii_alphabetic).collect();
    let flips = letters.windows(2).filter(|w| w[0].is_ascii_uppercase() != w[1].is_ascii_uppercase()).count();
    // Within each part ("noreply-shipping" is two), "y" counted as a vowel ("noreply").
    let consonants = local.split(|c: char| !c.is_ascii_alphabetic()).flat_map(|part| part.split(|c: char| "aeiouyAEIOUY".contains(c))).map(str::len).max().unwrap_or(0);
    letters.len() >= 5 && (flips >= 4 || consonants >= 5)
}

/// Services that send mail for other companies (shops, help desks, receipts): an organisation's name
/// on these is how it normally writes.
const PLATFORMS: [&str; 16] = [
    "shopify", "shopifyemail", "vify", "zendesk", "freshdesk", "helpscout", "intercom-mail", "stripe", "mcsv", "mailchimpapp", "sendgrid", "amazonses", "mandrillapp", "hubspotemail", "klaviyomail", "postmarkapp",
];

/// Hidden text below this is a newsletter's preheader, not a trick.
const HIDDEN_LIMIT: usize = 200;
const PROGRAMS: [&str; 30] = [
    "exe", "scr", "com", "pif", "bat", "cmd", "msi", "msix", "msp", "appx", "js", "jse", "vbs", "vbe", "wsf", "wsh", "ps1", "hta", "lnk", "jar", "reg", "cpl", "dll", "chm", "scf", "iso", "img", "vhd", "vhdx", "xll",
];
const PAGES: [&str; 5] = ["html", "htm", "shtml", "xhtml", "svg"];
const MACROS: [&str; 5] = ["docm", "xlsm", "pptm", "dotm", "xlam"];
const ARCHIVES: [&str; 7] = ["zip", "rar", "7z", "gz", "tgz", "cab", "ace"];
const DOCUMENTS: [&str; 10] = ["pdf", "doc", "docx", "xls", "xlsx", "jpg", "jpeg", "png", "txt", "mp4"];

/// A display name compared without case, quotes, separators or extra spaces ("Ada Lovelace" is
/// "ada.lovelace"); `None` for one too short to mean anything.
fn normal_name(name: &str) -> Option<String> {
    let name = name.trim_matches(['"', '\'', ' ']).split([' ', '.', '_']).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ").to_lowercase();
    (name.chars().filter(|c| c.is_alphabetic()).count() >= 3 && !name.contains('@')).then_some(name)
}

/// The address of "Name <address>", or the text itself.
fn bare(entry: &str) -> String {
    entry.rsplit_once('<').map_or(entry, |(_, a)| a.trim_end_matches('>')).trim().to_lowercase()
}

/// Two different domains that a reader would take for one: written alike once look-alike characters
/// are folded (see [`sender::skeleton`]) or one character apart, the same name under another suffix
/// ("acme-corp.net"), or the name with a word added ("acme-corp-billing.com"). Subdomains are left out:
/// "mail.acme-c0rp.com" is "acme-c0rp.com". Short domains are left to the name rules, too many real
/// ones being one letter apart; two public mailboxes ("yahoo.it", "yahoo.com") are not compared by name.
fn looks_like(known: &str, other: &str) -> bool {
    let known = sender::registrable(known);
    if known.is_empty() || sender::registrable(other) == known {
        return false;
    }
    // As many labels of the other as the known one has: "bank.co.ukk" for "bank.co.uk".
    let labels: Vec<&str> = other.split('.').collect();
    let tail = labels[labels.len().saturating_sub(known.split('.').count())..].join(".");
    let (k, t) = (sender::skeleton(&known), sender::skeleton(&tail));
    if known.chars().count().min(tail.chars().count()) >= 6 && (k == t || one_edit(&k, &t)) {
        return true;
    }
    let (name, theirs) = (sender::skeleton(&organisation_label(&known)), sender::skeleton(&organisation_label(other)));
    name.chars().count() >= 4 && !(sender::public_mail(&known) && sender::public_mail(other)) && format!("-{theirs}-").contains(&format!("-{name}-"))
}

/// One character added, removed, changed, or two neighbours swapped.
fn one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    let same_start = short.iter().zip(long.iter()).take_while(|(x, y)| x == y).count();
    match long.len() - short.len() {
        0 if same_start == short.len() => false,
        0 => {
            let rest = |from: usize| short[from..] == long[from..];
            rest(same_start + 1) || (same_start + 1 < short.len() && short[same_start] == long[same_start + 1] && short[same_start + 1] == long[same_start] && rest(same_start + 2))
        }
        1 => short[same_start..] == long[same_start + 1..],
        _ => false,
    }
}

/// `beavermail --screen-dry-run`: where the checks put each message of each Inbox, and why, printed.
/// Folders are synced read-only first so the Sent history is complete; nothing is moved and no mark is
/// saved.
pub fn dry_run() -> Result<(), String> {
    let vault = Vault::open(&accounts::data_dir()?, Box::new(SystemStore)).map_err(|e| e.to_string())?;
    // A tool for tuning the rules, not for users: with a passphrase the vault stays locked, and the app is
    // where it can be unlocked.
    if vault.is_locked() {
        return Err("the message vault is protected by a passphrase; the dry run works only with the key in the system keychain".into());
    }
    let archive = Arc::new(Store::open(&accounts::archive_path()?, vault.derived_key("index").as_deref()).map_err(|e| e.to_string())?);
    let vault = Arc::new(vault);
    let settings = accounts::load_settings()?;
    let list = accounts::load()?;
    let own: Vec<String> = list.iter().map(|a| a.address().to_owned()).collect();
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    println!("BeaverMail spam checks, dry run: nothing is moved on the server and no mark is saved.");

    for account in &list {
        let mut source = ImapSource::new(account, accounts::password(account).unwrap_or_default(), archive.clone(), vault.clone());
        if let Some(provider) = account.oauth {
            source = source.with_token(accounts::oauth::token_source(provider, account.clone()));
        }
        let key = account.key();
        eprintln!("{}: syncing the folder list, Sent and Inbox (read-only)…", account.address());
        let synced = runtime.block_on(async {
            source.mailboxes().await?;
            if let Some(sent) = archive.folder_with_role(&key, Role::Sent)? {
                // The whole Sent history, as far as one run allows: it decides who is known.
                for _ in 0..200 {
                    if source.sync(&sent).await?.remaining == 0 {
                        break;
                    }
                }
            }
            if let Some(inbox) = archive.folder_with_role(&key, Role::Inbox)? {
                source.sync(&inbox).await?;
            }
            Ok::<_, MailError>(())
        });
        if let Err(e) = synced {
            eprintln!("{}: could not sync ({e}); using what is stored", account.address());
        }

        let started = std::time::Instant::now();
        let known = Known::load(&archive, account, &own, &settings).map_err(|e| e.to_string())?;
        let loaded = started.elapsed();
        let Some(inbox) = archive.folder_with_role(&key, Role::Inbox).map_err(|e| e.to_string())? else {
            println!("\n== {}: no Inbox known yet ==", account.address());
            continue;
        };
        let messages = archive.messages(&key, &inbox).map_err(|e| e.to_string())?;
        println!("\n== {} — Inbox: {} messages, {} known addresses ==", account.address(), messages.len(), known.count());

        let (mut inbox_count, mut new_count, mut spam_count, mut flagged, mut headers_only, mut unchecked) = (0, 0, 0, 0, 0, 0);
        let (mut verified, mut failed) = (0, 0);
        let started = std::time::Instant::now();
        let verdicts = screen_all(&source, &known, &settings, &source.auth_server(), &messages, &[]);
        let checked = started.elapsed();
        for (m, screened) in messages.iter().zip(verdicts) {
            headers_only += usize::from(screened.verdict.is_some() && !screened.whole);
            unchecked += usize::from(screened.verdict.is_none());
            verified += usize::from(screened.verdict == Some(sender::Authenticity::Verified));
            failed += usize::from(screened.verdict == Some(sender::Authenticity::Failed));
            flagged += usize::from(!screened.findings.is_empty());
            let tag = match screened.place {
                Place::Spam => {
                    spam_count += 1;
                    "SPAM "
                }
                Place::Inbox if screened.new_sender => {
                    new_count += 1;
                    "NEW  "
                }
                Place::Inbox => {
                    inbox_count += 1;
                    "INBOX"
                }
            };
            println!("{tag}  {:16}  {:40}  {}  [{}]", m.received, cut(&format!("{} <{}>", m.from, m.address), 40), cut(&m.subject, 60), screened.why);
            for f in &screened.findings {
                println!("       {} {}", if f.level == Level::Danger { "!" } else { "·" }, f.text);
            }
        }
        println!(
            "\nSummary: {} in the Inbox ({inbox_count} from known senders, {new_count} from new ones), {spam_count} held as suspicious. {flagged} with warnings; {headers_only} checked on headers only (body not stored), {unchecked} not checked at all.",
            inbox_count + new_count
        );
        println!("Your server's verdict on the sender: {verified} verified, {failed} forged, {} without a verdict it wrote.", messages.len() - verified - failed);
        println!("Time: {} ms to learn who is known and train the model, {} ms to check {} messages.", loaded.as_millis(), checked.as_millis(), messages.len());
        let printable = messages.iter().filter(|m| fingerprint(&text_of(m)).is_some()).count();
        println!("Campaign fingerprints: {} of spam you have, {printable} of {} Inbox messages long enough to take one.", known.spam_prints(), messages.len());
        match known.model() {
            ((spam, ham), Some(size)) => println!("Learnt model: active, from {spam} spam and {ham} other messages, {size} weighted features."),
            ((spam, ham), None) => println!("Learnt model: off until it has {} examples of each kind (has {spam} spam, {ham} other).", learn::MIN_EXAMPLES),
        }
    }
    Ok(())
}

/// At most `n` characters, with an ellipsis when cut.
fn cut(text: &str, n: usize) -> String {
    if text.chars().count() <= n { text.to_owned() } else { format!("{}…", text.chars().take(n - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beavermail_core::Attachment;

    fn known() -> Known {
        let wrote: HashSet<String> = ["boss@acme-corp.com", "ada@example.org"].map(str::to_owned).into();
        let senders = [("Carlo Neri".to_owned(), "boss@acme-corp.com".to_owned()), ("Stranger".to_owned(), "x@spam.io".to_owned()), ("Ada Lovelace".to_owned(), "me@home.net".to_owned())];
        let mut settings = Settings::default();
        settings.set_mark("friend@new.org", Some(true));
        Known::new(wrote, &["me@home.net".to_owned()], &senders, &settings)
    }

    fn texts(found: &[Finding]) -> Vec<&str> {
        found.iter().map(|f| f.text.as_str()).collect()
    }

    #[test]
    fn known_senders_and_marks_decide_before_the_content() {
        let (known, mut settings) = (known(), Settings::default());
        settings.set_mark("friend@new.org", Some(true));
        settings.set_mark("pest@ads.com", Some(false));
        let at = |address: &str| place(&known, &settings, MessageId(1), address, false).0;
        assert_eq!(at("Boss@Acme-Corp.com"), Place::Inbox);
        assert_eq!(at("me@home.net"), Place::Inbox);
        assert_eq!(at("friend@new.org"), Place::Inbox);
        assert_eq!(at("pest@ads.com"), Place::Spam);
        let new = place(&known, &settings, MessageId(1), "x@spam.io", false);
        assert_eq!((new.0, new.2), (Place::Inbox, true), "having received mail does not make a sender known, but new is not spam");
        assert!(place(&known, &settings, MessageId(1), "", false).2);
        assert_eq!(place(&known, &settings, MessageId(1), "boss@acme-corp.com", true).0, Place::Spam, "a forged sender is held even when known");
        let mut known = known;
        known.replies.insert(MessageId(7));
        assert_eq!(place(&known, &settings, MessageId(7), "client@gmail.com", false).1, "a reply to a message you sent");
    }

    #[test]
    fn an_organisation_is_known_through_anyone_there_but_a_public_mailbox_is_not() {
        let wrote: HashSet<String> = ["sales@domainco.com", "pal@gmail.com"].map(str::to_owned).into();
        let known = Known::new(wrote, &["me@ownsite.example".to_owned()], &[], &Settings::default());
        let mut settings = Settings::default();
        let at = |settings: &Settings, address: &str| place(&known, settings, MessageId(1), address, false);
        assert_eq!(at(&settings, "orders@domainco.com").1, "you write to others at domainco.com");
        assert_eq!(at(&settings, "billing@mail.domainco.com").0, Place::Inbox, "a subdomain is the same organisation");
        assert_eq!(at(&settings, "noreply@web02.ownsite.example").0, Place::Inbox, "your own domain");
        assert!(at(&settings, "stranger@gmail.com").2, "anyone can open a Gmail mailbox: still new");
        assert_eq!(sender_key("Orders@E-Invoice.Partsco.com"), "@partsco.com");
        assert_eq!(sender_key("x@gmail.com"), "x@gmail.com");
        settings.set_mark("@partsco.com", Some(true));
        settings.set_mark("@spam.io", Some(false));
        settings.set_mark("vip@spam.io", Some(true));
        assert_eq!(at(&settings, "ventas@partsco.com").1, "you marked everyone at this domain as not spam");
        assert_eq!(at(&settings, "any@spam.io").0, Place::Spam);
        assert_eq!(at(&settings, "vip@spam.io").0, Place::Inbox, "a mark on the address wins over the domain");
    }

    #[test]
    fn a_known_name_on_a_stranger_address_and_a_look_alike_domain_are_dangerous() {
        let known = known();
        let body = MessageBody::default();
        let found = findings(&known, "\"carlo  neri\"", "carlo.neri@gmail.com", &body, "mail.home.net");
        assert!(texts(&found)[0].contains("someone you know at boss@acme-corp.com"), "{found:?}");
        assert!(findings(&known, "Carlo Neri", "boss@acme-corp.com", &body, "").is_empty(), "the real one");
        assert!(findings(&known, "Carlo Neri", "carlo@acme-corp.com", &body, "").is_empty(), "another address of the same organisation");
        assert!(findings(&known, "Registro.it", "boss@acme-corp.com", &body, "").is_empty(), "a known sender keeps the name it chose");
        assert_eq!(texts(&findings(&known, "ada.lovelace", "noreply@mailer-xq.example", &body, "")), ["It uses your own name, but comes from noreply@mailer-xq.example"]);
        assert!(findings(&known, "Ada Lovelace", "me@home.net", &body, "").is_empty(), "really you");
        assert!(findings(&known, "Ada Lovelace", "ada@home.net", &body, "").is_empty(), "another address of your own domain");
        let found = findings(&known, "Billing", "billing@acme-c0rp.com", &body, "");
        assert_eq!(texts(&found), ["The domain acme-c0rp.com looks like acme-corp.com, which you write to"]);
        assert!(!findings(&known, "Billing", "x@mail.acme-corp.com", &body, "").iter().any(|f| f.text.contains("looks like")), "a subdomain is the same site");
    }

    #[test]
    fn a_brand_from_someone_elses_domain_and_a_machine_made_address_are_flagged() {
        let known = known();
        let body = MessageBody::default();
        assert_eq!(texts(&findings(&known, "PayPal", "nick@cheap-batteries.example", &body, "")), ["The name says PayPal, but the message comes from cheap-batteries.example"]);
        assert!(texts(&findings(&known, "Your Garmin Member Bonus", "support@campus.example.br", &body, ""))[0].starts_with("The name says Garmin"));
        assert_eq!(texts(&findings(&known, "Acme-Corp Billing", "billing@acme-invoices.net", &body, "")), ["The name says Acme Corp, but the message comes from acme-invoices.net"], "an organisation you write to");
        for (name, address) in [("PayPal", "service@paypal.it"), ("Amazon.es", "auto-confirm@amazon.es"), ("Google Payments", "payments-noreply@google.com"), ("Microsoft", "x@accountprotection.microsoft.com"), ("Marketing team", "a@b.org"), ("Bianchi (UniCredit)", "e.b@unicredit.eu")] {
            assert!(findings(&known, name, address, &body, "").iter().all(|f| !f.text.starts_with("The name says")), "{name} <{address}>");
        }
        assert!(findings(&known, "Acme Corp Shop", "mailer@shopify.com", &body, "").is_empty(), "a platform sending for an organisation you know");
        assert_eq!(texts(&findings(&known, "Acme Corp", "acme.corp.billing@gmail.com", &body, "")), ["The name says Acme Corp, but the message comes from gmail.com"], "but not a mailbox anyone can open");
        assert!(!findings(&known, "PayPal", "mailer@shopify.com", &body, "").is_empty(), "but not for a listed brand");
        assert!(findings(&known, "billing.ing@agency.example", "billing.ing@agency.example", &body, "").is_empty(), "a name that is only the address");
        for local in ["WqOAXUVzQcKNliJ", "ewtkxdrhdafnwzj", "xkcdqwrt.bob"] {
            assert!(machine_made(local), "{local}");
        }
        for local in ["support", "noreply", "no-reply", "billing", "hostmaster", "kitchen", "fekurueunice123", "JohnSmith", "schmidt", "info", "noreplyclientes", "noreply-shipping", "receipts+acct_1PksddHJohyvID2c"] {
            assert!(!machine_made(local), "{local}");
        }
    }

    #[test]
    fn a_kept_verdict_reads_back_the_same() {
        let held = Screened {
            place: Place::Spam,
            why: "new sender: The name says PayPal".into(),
            key: "@cumulus.com".into(),
            new_sender: true,
            findings: vec![Finding { level: Level::Danger, text: "The name says PayPal".into() }],
            verdict: Some(sender::Authenticity::Failed),
            whole: false,
        };
        let back: Screened = serde_json::from_str(&serde_json::to_string(&held).unwrap()).unwrap();
        assert_eq!((back.place, back.key, back.findings, back.verdict), (held.place, held.key, held.findings, held.verdict));
    }

    #[test]
    fn one_campaign_under_many_names_has_one_fingerprint() {
        let beauty = fingerprint("A Quick Bedtime Ritual Morning Beauty Dispatch: The simple Korean pink gelatin ritual lifting mature skin naturally... 57-Ye").unwrap();
        let glow = fingerprint("The 3-Minute Nighttime Ritual Morning Beauty Dispatch: The simple Korean pink gelatin ritual lifting mature skin naturally... 57-Ye").unwrap();
        let invoice = fingerprint("Your Scaleway invoice #3379055 for September 2026 Hello, Please find attached your monthly invoice").unwrap();
        assert!(similarity(&beauty, &glow) >= SAME_CAMPAIGN, "{}", similarity(&beauty, &glow));
        assert!(similarity(&beauty, &invoice) < 0.2);
        assert!(fingerprint("Hi there").is_none(), "too short to tell");
    }

    #[test]
    fn look_alike_domains_are_caught_but_short_distant_or_own_ones_are_not() {
        for (a, b) in [
            ("paypal.com", "paypa1.com"),
            ("example.org", "exarnple.org"),
            ("acme-corp.com", "acme-crop.com"),
            ("bank.co.uk", "bank.co.ukk"),
            ("acme-corp.com", "acmecorp.com"),
            ("acme-corp.com", "mail.acme-c0rp.com"),
            ("acme-corp.com", "acme-corp.net"),
            ("acme-corp.com", "acme-corp.co.uk"),
            ("acme-corp.com", "acme-corp-billing.com"),
            ("acme-corp.com", "secure-acme-corp.com"),
            ("mail.acme-corp.com", "acme-c0rp.com"),
            ("paypal.com", "pаypal.com"),
            ("paypal.com", "xn--pypal-4ve.com"),
            ("apple.com", "аррӏе.com"),
            ("gmail.com", "gmail-support.com"),
        ] {
            assert!(looks_like(a, b), "{a} {b}");
        }
        for (a, b) in [
            ("gmx.de", "gmx.at"),
            ("x.org", "y.org"),
            ("google.com", "github.com"),
            ("same.com", "same.com"),
            ("acme-corp.com", "mail.acme-corp.com"),
            ("yahoo.com", "yahoo.it"),
            ("acme.com", "acmeplumbing.com"),
        ] {
            assert!(!looks_like(a, b), "{a} {b}");
        }
    }

    #[test]
    fn links_reply_to_hidden_text_and_attachments_are_checked() {
        let known = known();
        let file = |name: &str| Attachment { index: 0, name: name.into(), mime: String::new(), size: 1 };
        let body = MessageBody {
            html: Some(format!(r#"<a href="https://evil.io/x">www.bank.com</a> <a href="http://192.168.1.4/a">here</a> <div style="display:none">{}</div>"#, "spam words ".repeat(30))),
            reply_to: vec!["Desk <desk@other.net>".into(), "Desk <desk.shop@gmail.com>".into()],
            attachments: vec![file("invoice.pdf.exe"), file("login.html"), file("budget.xlsm"), file("photos.zip"), file("report.pdf")],
            ..MessageBody::default()
        };
        let found = findings(&known, "Shop", "news@shop.com", &body, "");
        let all = texts(&found);
        assert!(all.contains(&"A link shows www.bank.com but opens evil.io"), "{all:?}");
        let tracked = MessageBody { html: Some(r#"<a href="https://lmj.shop.net/c">www.shop.com</a>"#.into()), ..MessageBody::default() };
        assert!(findings(&known, "Shop", "news@shop.net", &tracked, "").is_empty(), "the sender's own tracking domain");
        let mailer = |auth: Vec<beavermail_core::AuthResult>| MessageBody { html: Some(r#"<a href="https://u123.ct.mailer.example/ls">my.shop.com</a>"#.into()), auth, ..MessageBody::default() };
        let dkim = vec![beavermail_core::AuthResult { server: "mx.home.net".into(), checks: vec![beavermail_core::AuthCheck { method: "dkim".into(), result: "pass".into(), domain: "news.shop.com".into() }] }];
        assert!(findings(&known, "Shop", "x@news.shop.com", &mailer(dkim), "mx.home.net").is_empty(), "a verified sender showing its own site through its mailer's tracking");
        assert!(!findings(&known, "Shop", "x@news.shop.com", &mailer(Vec::new()), "mx.home.net").is_empty(), "but not unverified");
        let redirected = |to: &str| MessageBody { html: Some(format!(r#"<a href="https://www.google.com/url?q={to}">www.bank.com</a>"#)), ..MessageBody::default() };
        assert!(findings(&known, "Shop", "news@shop.com", &redirected("https%3A%2F%2Fbank.com%2Flogin"), "").is_empty(), "through a redirect to the site it names");
        assert_eq!(texts(&findings(&known, "Shop", "news@shop.com", &redirected("https%3A%2F%2Fevil.io%2Fx"), "")), ["A link shows www.bank.com but opens evil.io"], "named by where it really leads");
        assert!(all.contains(&"A link opens 192.168.1.4, a bare network address instead of a site"));
        assert!(all.contains(&"Replies would go to desk.shop@gmail.com, not to shop.com"), "only the public mailbox counts");
        let same = MessageBody { reply_to: vec!["noreply@orange.es".into()], ..MessageBody::default() };
        assert!(findings(&known, "Orange", "orange@orange.es", &same, "").iter().all(|f| !f.text.starts_with("Replies")), "same domain");
        assert!(all.iter().any(|t| t.starts_with("It hides ")));
        assert!(all.contains(&"invoice.pdf.exe is a program or script disguised as a .pdf file: opening it can run code on this computer"));
        assert!(all.iter().any(|t| t.starts_with("login.html is a web page")));
        assert!(all.iter().any(|t| t.starts_with("budget.xlsm is an Office file")));
        assert!(all.iter().any(|t| t.starts_with("photos.zip is an archive")));
        assert!(!all.iter().any(|t| t.starts_with("report.pdf")));
        assert!(found.windows(2).all(|w| w[0].level >= w[1].level), "most serious first");
        let hosted = MessageBody { html: Some(r#"<a href="https://storage.googleapis.com/sfghjklm/l.html">3-minute ritual</a>"#.into()), spam_tests: vec!["BAYES_00".into(), "SPF_FAIL".into()], ..MessageBody::default() };
        assert_eq!(
            texts(&findings(&known, "Beauty Wire", "support@gym-gear.example", &hosted, "")),
            ["Your mail server found that gym-gear.example did not authorise the computer that sent this (SPF failed)", "A link opens a page uploaded to storage.googleapis.com, a common way to hide a phishing or scam page"]
        );
        assert!(!texts(&findings(&known, "Ada", "ada@example.org", &hosted, "")).iter().any(|t| t.contains("uploaded")), "a known sender may share a file");
        let from_known = findings(&known, "Ada", "ada@example.org", &MessageBody { attachments: vec![file("photos.zip")], ..MessageBody::default() }, "");
        assert!(from_known.is_empty(), "an archive from someone you write to is normal");
    }
}
