// SPDX-License-Identifier: AGPL-3.0-only
// A release build on Windows opens no console window beside the app (a debug build keeps it, for its output).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! BeaverMail desktop shell: accounts with their folders, a message list and a reader.
//! Accounts come from `accounts.toml` and the system credential store (see
//! `beavermail-accounts`). Mail calls run on a background runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

mod actions;
mod attachments;
mod books;
mod calendars;
mod calview;
mod card;
mod commands;
mod compose;
mod diag;
mod drafts;
mod event_form;
mod encryption;
mod original;
mod remote;
mod screen;
mod sender;
mod tabs;
mod html;
mod icons;
mod learn;
mod security;
mod theme;
mod threads;
mod unsubscribe;
mod views;

use beavermail_accounts as store;
use beavermail_core::{OAuthProvider, Account, Density, DisplayMode, Hit, Keep, MailError, MailSender, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Progress, Role, Server, Settings, SyncReport, Tls};
use beavermail_imap::ImapSource;
use beavermail_smtp::{SmtpSender, render_draft};
use beavermail_store::Store;
use beavermail_vault::{Mode, SystemStore, Vault};
use chrono::Datelike;
use eframe::egui::{self, Align, Layout, RichText, pos2, vec2};

use crate::actions::{Action, OFFLINE_CHANGES, Pending, UNDO_SECS, perform};
use crate::commands::Command;
use crate::icons::Icon;
use tokio::runtime::Runtime;

/// Result of a background mail call. Tagged with the account's serial, so answers
/// for an account that was removed meanwhile are dropped.
enum Event {
    /// The folders of an account. The flag is true when the answer came from the server.
    Mailboxes(u64, Result<Vec<Mailbox>, MailError>, bool),
    /// The server's folders, asked again on its own after the account went offline.
    Retried(u64, Result<Vec<Mailbox>, MailError>),
    /// The background download of an account ended.
    Downloaded(u64, Result<(), MailError>),
    /// The server says something changed in the Inbox of an account.
    Changed(u64),
    /// Search results from one account: (search number, account, hits).
    Found(u64, u64, Result<Vec<Hit>, MailError>),
    /// One sync of the selected folder finished; more history may remain.
    Synced(u64, usize, Result<SyncReport, MailError>),
    Messages(u64, usize, Result<Vec<MessageSummary>, MailError>),
    /// The Sent folder of the account shown, for the user's replies in its conversations: (account, folder index shown, messages).
    SentList(u64, usize, Result<Vec<MessageSummary>, MailError>),
    Body(u64, MessageId, Result<Box<Prepared>, MailError>),
    /// The text of an open message of the conversation that is not in focus.
    Unfolded(MessageId, Result<Box<Prepared>, MailError>),
    /// The text of a message to answer, read without opening it: (account, message, action, result).
    Answer(u64, MessageId, Action, Result<Box<MessageBody>, MailError>),
    /// The raw message asked for by View source.
    Source(MessageId, Result<Vec<u8>, MailError>),
    /// The checks' verdicts on the Inbox loaded as (account, folder index).
    Screened(u64, usize, Result<std::collections::HashMap<MessageId, screen::Screened>, MailError>),
    /// A write (read flag, archive, trash) finished; the UI already showed its result.
    /// The third field is the message and action, for a change that can wait for the connection.
    Done(u64, usize, Option<(MessageId, Action)>, Result<(), MailError>),
    /// A folder was created in account `serial`, with its full name, or the server refused it.
    FolderMade(u64, Result<String, MailError>),
    /// Folder (account, old name) was renamed, to this full name, or the server refused it.
    FolderRenamed(u64, String, Result<String, MailError>),
    /// Folder (account, name) was deleted with those inside it, or the server refused it.
    FolderDeleted(u64, String, Result<(), MailError>),
    /// Folder (account, name) went into the trash, under this full name, or the server refused it.
    FolderTrashed(u64, String, Result<String, MailError>),
    /// A message was sent, or the server refused it: (draft id, result, why the copy in Sent could not
    /// be saved when the message itself went out). The draft is the compose window's or one in the Outbox.
    Sent(String, Result<(), MailError>, Option<String>),
    /// A change to how stored mail is protected finished.
    Vault(VaultOutcome),
    /// The bytes of an attachment, to open with another program.
    Attachment(Result<(String, Vec<u8>), MailError>),
    /// Saving attachments through a file dialog ended: `None` when the user cancelled, else what to tell them.
    Saved(Result<Option<String>, String>),
    /// Files chosen in the "attach" dialog.
    Picked(Vec<std::path::PathBuf>),
    /// The copy of a draft on the server was stored (or not): (account serial, draft id, what the text was, result).
    DraftPushed(u64, String, u64, Result<(), MailError>),
    /// A copy of an attachment was made in the vault for a draft: (draft id, where in the vault, result).
    FileStored(String, std::path::PathBuf, Result<(), String>),
    /// A one-click unsubscribe ended: (who, result).
    Unsubscribed(String, Result<(), String>),
    /// A sign-in with a provider ended: (its cancel flag, which tells it from an earlier one, result).
    SignedIn(Arc<std::sync::atomic::AtomicBool>, Result<store::oauth::Grant, String>),
    /// A "Test connection" ended: (the form it is for, what was tested, IMAP's answer, SMTP's answer).
    Checked(Option<u64>, Box<(Account, String, String)>, Result<(), String>, Result<(), String>),
    /// A key file was picked to import for this address (`None`: the dialog was cancelled).
    KeyFile(String, Option<(String, Vec<u8>)>),
    /// A picture chosen for the card being edited, made small; `None` when none was chosen.
    ContactPhoto(Option<Result<Vec<u8>, String>>),
    /// An address book was synced: (its URL, what moved, or why not).
    ContactsSynced(String, Result<beavermail_carddav::Synced, MailError>),
    /// A calendar was synced: (its URL, how many events came, or why not).
    CalendarsSynced(String, Result<usize, MailError>),
    /// An event was written to (or deleted from) a calendar on its server, and kept here.
    CalendarWritten(Result<(), MailError>),
    /// "Find calendars" answered.
    CalendarsFound(Result<Vec<beavermail_carddav::Found>, String>),
    /// "Find address books" answered: (URL, name) of each, or why none.
    BooksFound(Result<Vec<beavermail_carddav::Found>, String>),
    /// Everyone the user has written to, for the address fields.
    Contacts(Result<Vec<beavermail_store::Contact>, MailError>),
}

/// A sign-in with a provider waiting for the user in the browser.
struct SignIn {
    provider: OAuthProvider,
    /// The account page it is for; `None` for the Add account dialog.
    editing: Option<u64>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl SignIn {
    fn stop(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What the user pressed on an account's page in Settings.
enum PageAction {
    Remove,
    Test,
    SignIn(OAuthProvider),
    CancelSignIn,
}

enum VaultOutcome {
    Applied(Mode, Result<(), MailError>),
    Unlocked(Result<(), MailError>),
    Reset(Result<(), MailError>),
    Recovered(Result<(), MailError>),
}

/// How long an offline account waits before its server is asked again, doubling up to the most.
const RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(15);
const RETRY_MOST: std::time::Duration = std::time::Duration::from_secs(300);

/// The command palette (Ctrl+K): what is typed and which line is chosen.
#[derive(Default)]
struct Palette {
    query: String,
    at: usize,
    /// The user moved with the arrows; until then the first line that can run is chosen.
    moved: bool,
    /// Move to…: the messages to move; it lists only the folders of their account then.
    moving: Vec<MessageId>,
}

/// A line of the palette: a command, or a folder to go to or move to as (account, folder index).
#[derive(Clone)]
enum Entry {
    Run(Command),
    Folder(u64, usize),
    MoveTo(u64, usize),
}

/// The New folder dialog: the account, the folder it goes inside (by name; `None` at the top), the
/// name typed, whether the server is creating it, and why it refused.
struct NewFolder {
    serial: u64,
    parent: Option<String>,
    name: String,
    busy: bool,
    error: Option<String>,
    /// Rename…: the folder's full name; only its own name changes, where it is.
    renaming: Option<String>,
}

/// Messages being dragged from the list onto a folder.
struct Dragged(Vec<MessageId>);

/// Which pane the keys act on: the list (arrows move through it) or the reader (arrows scroll it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    List,
    Reader,
}

/// A scroll of the reader asked for from the keyboard.
#[derive(Clone, Copy)]
enum Scroll {
    /// Lines down (negative: up).
    Lines(f32),
    /// Screens down (negative: up).
    Pages(f32),
    /// To the top, or with `true` to the end.
    Edge(bool),
}

/// The one spam correction the reader offers for the open message.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Correction {
    /// Held as suspicious: let the sender back into the Inbox.
    NotSpam,
    /// From a new sender and in the Inbox: move it, and the sender's other mail, to Spam.
    Spam,
    /// In the Spam folder: back to the Inbox, sender marked as not spam.
    OutOfSpam,
}

/// What the user did in the sidebar this frame.
#[derive(Default)]
struct SidebarOutcome {
    chosen: Option<(u64, usize)>,
    /// The suspicious mail under this (account, Inbox index) was clicked.
    suspicious: Option<(u64, usize)>,
    /// "Check all mail for spam" was chosen in this (account, Inbox index)'s menu.
    recheck: Option<(u64, usize)>,
    /// "Empty Spam" or "Empty Trash" was chosen in this (account, folder index)'s menu.
    empty: Option<(u64, usize)>,
    /// "Check all mail for spam" was chosen in the menu of All inboxes or of every account's Suspicious.
    recheck_all: bool,
    /// "Empty Spam" was chosen in the menu of every account's Spam.
    empty_all: bool,
    removed: Option<u64>,
    edit: Option<u64>,
    add: bool,
    /// "All inboxes" was clicked.
    all: bool,
    /// "Scheduled" was clicked.
    scheduled: bool,
    /// The suspicious mail of every account, under All inboxes, was clicked.
    all_suspicious: bool,
    /// The Spam of every account, under All inboxes, was clicked.
    all_spam: bool,
    /// Messages dropped onto (account, folder index).
    dropped: Option<(Vec<MessageId>, u64, usize)>,
    /// "New folder…" was chosen in this (account, folder index)'s menu.
    new_inside: Option<(u64, usize)>,
    /// "Rename…" or "Delete folder…" was chosen in this (account, folder index)'s menu.
    rename: Option<(u64, usize)>,
    delete: Option<(u64, usize)>,
}

/// A message as the server holds it, shown in a source tab.
struct SourceView {
    id: MessageId,
    subject: String,
    /// Its lines, once read; or why it could not be.
    lines: Option<Result<Vec<String>, String>>,
    size: usize,
    /// The find bar, while open.
    find: Option<Find>,
}

/// Finding text in a source tab.
#[derive(Default)]
struct Find {
    query: String,
    /// What `hits` were found for.
    searched: String,
    /// Each match as (line, start, end), in bytes of the line, in order.
    hits: Vec<(usize, usize, usize)>,
    /// The match shown.
    at: usize,
    /// Scroll to the match shown on the next frame.
    jump: bool,
    /// Put the keyboard in the field on the next frame.
    focus: bool,
}

/// Fewer matches than this are all shown; past it the count says "more than".
const MAX_HITS: usize = 10_000;

/// Where `query` occurs in `lines`, ignoring the case of ASCII letters (a source is mostly ASCII, and
/// so a match keeps the byte positions of the line it is in).
fn find_hits(lines: &[String], query: &str) -> Vec<(usize, usize, usize)> {
    let query = query.to_ascii_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        let mut from = 0;
        while let Some(start) = lower[from..].find(&query).map(|s| from + s) {
            hits.push((at, start, start + query.len()));
            if hits.len() >= MAX_HITS {
                return hits;
            }
            from = start + query.len();
        }
    }
    hits
}

/// The find bar of a source tab: the field, the count, previous and next. Enter goes to the next
/// match, Shift+Enter (or Shift+F3) to the one before; Esc closes. Returns true to close it.
fn find_bar(ui: &mut egui::Ui, t: &theme::Tokens, find: &mut Find, lines: &[String]) -> bool {
    let mut close = false;
    let mut step = 0isize;
    ui.horizontal(|ui| {
        let field = ui.add(sized_field(&mut find.query, 300.0).hint_text("Find in source"));
        if std::mem::take(&mut find.focus) {
            field.request_focus();
        }
        let (enter, escape, shift) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape), i.modifiers.shift));
        if field.lost_focus() && escape {
            close = true;
        } else if field.lost_focus() && enter {
            step = if shift { -1 } else { 1 };
            field.request_focus();
        }
        if ui.input(|i| i.key_pressed(egui::Key::F3)) {
            step = if shift { -1 } else { 1 };
        }
        let count = match find.hits.len() {
            _ if find.query.is_empty() => String::new(),
            0 => "No matches".to_owned(),
            MAX_HITS => format!("{} of more than {MAX_HITS}", find.at + 1),
            n => format!("{} of {n}", find.at + 1),
        };
        ui.label(RichText::new(count).size(13.0).color(t.text_dim));
        let some = !find.hits.is_empty();
        if icons::button(ui, t, Icon::ChevronUp, "Previous match (Shift+Enter)", some, false).clicked() {
            step = -1;
        }
        if icons::button(ui, t, Icon::ChevronDown, "Next match (Enter)", some, false).clicked() {
            step = 1;
        }
        close |= icons::button(ui, t, Icon::X, "Close (Esc)", true, false).clicked();
    });
    if find.query != find.searched {
        find.hits = find_hits(lines, &find.query);
        find.searched = find.query.clone();
        (find.at, find.jump) = (0, true);
    } else if step != 0 && !find.hits.is_empty() {
        let n = find.hits.len() as isize;
        find.at = (find.at as isize + step).rem_euclid(n) as usize;
        find.jump = true;
    }
    close
}

/// A message of the open conversation shown open beside the tab's own: its text, once read, or why
/// it could not be.
struct Unfolded {
    id: MessageId,
    body: Option<Result<OpenBody, String>>,
}

/// What occurrences were worked out for: (first day, days, the calendars shown, version).
type CalendarKey = (chrono::NaiveDate, i64, String, u64);

/// The open message's body, already reduced to blocks for both display modes.
struct OpenBody {
    data: MessageBody,
    plain: Vec<html::Block>,
    /// `None` when the message has no HTML part.
    simplified: Option<Vec<html::Block>>,
    /// The HTML drawn as its sender styled it, made when first shown (and again for another width).
    original: Option<original::Page>,
    /// The user agreed to load remote content for this message.
    remote: bool,
    /// The HTML links to images, styles or fonts on a server.
    has_remote: bool,
    /// Images that only report the message was opened: never loaded (see [`html::trackers`]).
    trackers: Vec<String>,
    /// What deserves a second look about the message, the most serious first.
    findings: Vec<screen::Finding>,
    /// Where the earlier messages it carries start, in `plain` and in `simplified` (see [`html::quoted_tail`]).
    folds: (Option<usize>, Option<usize>),
    /// "Show quoted text" was pressed.
    quoted: bool,
}

impl OpenBody {
    fn new(p: Prepared) -> Self {
        Self { data: p.data, plain: p.plain, simplified: p.simplified, original: None, remote: false, has_remote: p.has_remote, trackers: p.trackers, findings: p.findings, folds: p.folds, quoted: false }
    }
}

/// An [`OpenBody`] without what is drawn on screen, made in the background: parsing the HTML and
/// running the sender checks take long enough to make the list stutter.
struct Prepared {
    data: MessageBody,
    plain: Vec<html::Block>,
    simplified: Option<Vec<html::Block>>,
    has_remote: bool,
    trackers: Vec<String>,
    findings: Vec<screen::Finding>,
    folds: (Option<usize>, Option<usize>),
}

impl Prepared {
    fn new(data: MessageBody, findings: Vec<screen::Finding>) -> Self {
        let plain = html::plain_blocks(&data.text);
        let simplified = data.html.as_deref().map(html::blocks);
        let has_remote = data.html.as_deref().is_some_and(html::has_remote);
        let trackers = data.html.as_deref().map(html::trackers).unwrap_or_default();
        let folds = (html::quoted_tail(&plain), simplified.as_deref().and_then(html::quoted_tail));
        Self { data, plain, simplified, has_remote, trackers, findings, folds }
    }
}

/// What the user did in the reader this frame.
#[derive(Default)]
struct ReaderOutcome {
    action: Option<(MessageId, Action)>,
    /// A link that was clicked, cleaned of tracking parameters.
    url: Option<String>,
    /// A display mode picked for one message only.
    mode: Option<(MessageId, DisplayMode)>,
    /// A display mode to remember for a sender: (address, mode).
    remember: Option<(String, DisplayMode)>,
    /// An attachment of a message to open or save: its position and what to do.
    attachment: Option<(MessageId, u32, views::ChipAction)>,
    /// "Save all" or "Save selected" was pressed on this message.
    save_many: Option<MessageId>,
    /// Remote content may always (or never) be loaded for this sender: (address, allowed).
    allow_sender: Option<(String, bool)>,
    /// The spam correction the user chose for the open message.
    mark: Option<Correction>,
    /// A command for all the messages picked in the list.
    bulk: Option<Action>,
    /// Move to folder… was pressed, for the open message or those picked.
    move_to: bool,
    /// Unsubscribe was pressed: who sent the message, and how to leave the list.
    unsubscribe: Option<(String, unsubscribe::Method)>,
    /// Block sender was pressed on this message.
    block: Option<MessageId>,
    /// A closed message of the conversation was opened.
    unfold: Option<MessageId>,
    /// An open message of the conversation was closed.
    collapse: Option<MessageId>,
    /// "Show earlier" was pressed in the conversation.
    expand: bool,
    /// Trust was pressed for this message's sender.
    trust: Option<MessageId>,
    /// The sender's name was clicked: open their card, as (address, name).
    card: Option<(String, String)>,
    /// "Try again" on a message that could not be opened.
    retry: bool,
    /// View source was chosen for this message.
    source: Option<MessageId>,
    /// A choice on the invitation a message carries.
    invite: Option<(MessageId, calview::Invite)>,
}

/// What the user did on one open message of the reader.
#[derive(Default)]
struct Said {
    answer: Option<Action>,
    collapse: bool,
    url: Option<String>,
    attachment: Option<(u32, views::ChipAction)>,
    save_many: bool,
    mode: Option<DisplayMode>,
    remember: Option<DisplayMode>,
    allow_sender: Option<bool>,
    unsubscribe: Option<(String, unsubscribe::Method)>,
    card: Option<(String, String)>,
    mark: Option<Correction>,
    trust: bool,
    source: bool,
    invite: Option<calview::Invite>,
}

impl ReaderOutcome {
    /// Adds what was done on open message `m`.
    fn take(&mut self, m: &MessageSummary, said: Said) {
        let id = m.id;
        self.action = said.answer.map(|a| (id, a)).or(self.action.take());
        self.collapse = said.collapse.then_some(id).or(self.collapse);
        self.url = said.url.or(self.url.take());
        self.attachment = said.attachment.map(|(index, what)| (id, index, what)).or(self.attachment);
        self.save_many = said.save_many.then_some(id).or(self.save_many);
        self.mode = said.mode.map(|mode| (id, mode)).or(self.mode);
        self.remember = said.remember.map(|mode| (m.address.clone(), mode)).or(self.remember.take());
        self.allow_sender = said.allow_sender.map(|allow| (m.address.clone(), allow)).or(self.allow_sender.take());
        self.unsubscribe = said.unsubscribe.or(self.unsubscribe.take());
        self.card = said.card.or(self.card.take());
        self.mark = said.mark.or(self.mark);
        self.trust = said.trust.then_some(id).or(self.trust);
        self.source = said.source.then_some(id).or(self.source);
        self.invite = said.invite.map(|i| (id, i)).or(self.invite.take());
    }
}

impl AccountView {
    /// The server's separator between a folder and those inside it, the same for all its folders.
    fn delimiter(&self) -> Option<char> {
        self.mailboxes.iter().find_map(|m| m.delimiter)
    }

    /// Whether folder `name` is Trash or Spam, or inside one: no place for new folders.
    fn in_trash_or_spam(&self, name: &str) -> bool {
        self.mailboxes.iter().filter(|m| matches!(m.role, Role::Trash | Role::Junk)).any(|m| m.name == name || views::is_inside(name, &m.name, m.delimiter))
    }
}

struct AccountView {
    /// Never reused within a run, unlike a position in the list.
    serial: u64,
    account: Account,
    /// `None` when the password could not be read; `error` says why.
    source: Option<Arc<ImapSource>>,
    sender: Option<Arc<SmtpSender>>,
    mailboxes: Vec<Mailbox>,
    error: Option<String>,
    /// Why the server could not be reached the last time it was asked; the archive is shown meanwhile.
    offline: Option<String>,
    /// The background download, shared with the status bar.
    progress: Arc<Progress>,
    /// Set to stop the Inbox watcher; `watching` says whether one is running.
    watch_stop: Arc<std::sync::atomic::AtomicBool>,
    watching: bool,
    /// Whether the server is to be asked again on its own, and after how long the next time.
    retrying: bool,
    retry_wait: std::time::Duration,
}

/// The "Add account" form. Ports are text until submitted.
struct NewAccount {
    /// Serial of the account being edited; `None` when adding.
    editing: Option<u64>,
    name: String,
    user: String,
    /// The address mail is sent from. May stay empty when the login is itself an address.
    email: String,
    /// Empty while editing means "keep the stored password".
    password: String,
    imap_host: String,
    imap_port: String,
    imap_tls: Tls,
    /// Empty means the same host as IMAP.
    smtp_host: String,
    smtp_port: String,
    smtp_tls: Tls,
    /// The outgoing server signs in with its own user and password.
    smtp_own: bool,
    smtp_user: String,
    /// Empty while editing means "keep the stored one".
    smtp_password: String,
    save_sent: bool,
    keep: Keep,
    signature: String,
    /// Signed in with a provider: kept through an edit, so the account stays an OAuth one.
    oauth: Option<OAuthProvider>,
    /// The last "Test connection", shown while the form still says what was tested.
    check: Option<Check>,
    error: Option<String>,
    /// Focus the first field on the first frame only.
    focus_first: bool,
}

impl Default for NewAccount {
    fn default() -> Self {
        Self {
            editing: None,
            name: String::new(),
            user: String::new(),
            email: String::new(),
            password: String::new(),
            imap_host: String::new(),
            imap_port: "143".into(),
            imap_tls: Tls::StartTls,
            smtp_host: String::new(),
            smtp_port: "587".into(),
            smtp_tls: Tls::StartTls,
            smtp_own: false,
            smtp_user: String::new(),
            smtp_password: String::new(),
            save_sent: true,
            keep: Keep::default(),
            signature: String::new(),
            oauth: None,
            check: None,
            error: None,
            focus_first: true,
        }
    }
}

/// A "Test connection": the account and password as typed when it started, and what the incoming and
/// outgoing servers answered (`None` while it runs).
struct Check {
    tested: (Account, String, String),
    answers: Option<(Result<(), String>, Result<(), String>)>,
}

impl NewAccount {
    /// The test that applies to the form as it is now; one made before a change is out of date.
    fn current_check(&self) -> Option<&Check> {
        self.check.as_ref().filter(|c| self.build().is_ok_and(|a| a == c.tested.0) && self.password == c.tested.1 && self.smtp_password == c.tested.2)
    }

    /// The form filled with an existing account's settings; the password is left empty.
    fn editing(serial: u64, account: &Account) -> Self {
        let port = |s: &Server| s.port.to_string();
        Self {
            editing: Some(serial),
            name: account.name.clone(),
            user: account.user.clone(),
            email: account.email.clone(),
            imap_host: account.imap.host.clone(),
            imap_port: port(&account.imap),
            imap_tls: account.imap.tls,
            smtp_host: if account.smtp.host == account.imap.host { String::new() } else { account.smtp.host.clone() },
            smtp_port: port(&account.smtp),
            smtp_tls: account.smtp.tls,
            smtp_own: account.smtp_user.is_some(),
            smtp_user: account.smtp_user.clone().unwrap_or_default(),
            save_sent: account.save_sent,
            keep: account.keep,
            signature: account.signature.clone(),
            oauth: account.oauth,
            focus_first: true,
            ..Self::default()
        }
    }

    /// Whether saving would change `account`: an invalid form counts as a change, so it is never dropped silently.
    fn changed(&self, account: &Account) -> bool {
        !self.password.is_empty() || !self.smtp_password.is_empty() || self.build().map_or(true, |built| built != *account)
    }

    /// Whether the required fields are filled in.
    fn ready(&self) -> bool {
        let password = self.editing.is_some() || !self.password.is_empty();
        !self.user.trim().is_empty() && !self.imap_host.trim().is_empty() && password
    }

    /// The form for the account `grant` signed in to, with the provider's servers.
    fn signed_in(provider: OAuthProvider, grant: store::oauth::Grant) -> Self {
        let (imap, smtp) = store::oauth::servers(provider);
        Self {
            user: grant.email.clone(),
            email: grant.email,
            password: grant.refresh_token,
            imap_host: imap.host,
            imap_port: imap.port.to_string(),
            imap_tls: imap.tls,
            smtp_host: smtp.host,
            smtp_port: smtp.port.to_string(),
            smtp_tls: smtp.tls,
            name: grant.name.unwrap_or_default(),
            save_sent: false, // both providers file sent mail themselves
            oauth: Some(provider),
            ..Self::default()
        }
    }

    fn build(&self) -> Result<Account, String> {
        let port = |text: &str, what: &str| text.trim().parse().map_err(|_| format!("{what} port is not a number"));
        let (user, imap_host) = (self.user.trim(), self.imap_host.trim());
        if !self.ready() {
            return Err("user, password and IMAP host are required".into());
        }
        let email = match self.email.trim() {
            "" if user.contains('@') => user,
            "" => return Err("enter the email address to send from".into()),
            email if email.contains('@') => email,
            _ => return Err("the email address needs an @".into()),
        };
        let smtp_host = if self.smtp_host.trim().is_empty() { imap_host } else { self.smtp_host.trim() };
        let smtp_user = match self.smtp_user.trim() {
            _ if !self.smtp_own || self.oauth.is_some() => None,
            "" => return Err("enter the user for the outgoing server".into()),
            user => Some(user.to_owned()),
        };

        Ok(Account {
            name: self.name.trim().to_owned(),
            user: user.to_owned(),
            email: email.to_owned(),
            imap: Server { host: imap_host.to_owned(), port: port(&self.imap_port, "IMAP")?, tls: self.imap_tls },
            smtp: Server { host: smtp_host.to_owned(), port: port(&self.smtp_port, "SMTP")?, tls: self.smtp_tls },
            save_sent: self.save_sent,
            keep: self.keep,
            signature: self.signature.trim_end().to_owned(),
            oauth: self.oauth,
            smtp_user,
        })
    }
}

/// Someone in the Contacts section: a card of a synced address book (with the other address books
/// that have one of its addresses, by URL), or a contact kept only here.
#[derive(Clone)]
enum Person {
    Card(beavermail_store::StoredCard, Vec<String>),
    Saved(beavermail_store::Contact),
}

impl Person {
    fn name(&self) -> &str {
        match self {
            Self::Card(c, _) => &c.name,
            Self::Saved(c) => &c.name,
        }
    }

    /// The name, or an address or a phone for an unnamed one.
    fn title(&self) -> String {
        match self {
            _ if !self.name().is_empty() => self.name().to_owned(),
            Self::Card(c, _) => c.emails.first().or(c.phones.first()).cloned().unwrap_or_default(),
            Self::Saved(c) => c.address.clone(),
        }
    }

    /// The row's two lines: the name, then an address or a phone and, among several address books,
    /// which one they are in (`own`, left out when one is shown) and the others that have them too.
    fn lines(&self, books: &[beavermail_core::AddressBook], own: bool) -> (String, String) {
        let reach = match self {
            Self::Card(c, _) => c.emails.first().or(c.phones.first()).cloned().unwrap_or_default(),
            Self::Saved(c) => c.address.clone(),
        };
        let mut second = if self.name().is_empty() { String::new() } else { reach };
        let mut add = |part: String| {
            if !second.is_empty() {
                second.push_str("  ·  ");
            }
            second.push_str(&part);
        };
        let name_of = |url: &str| books.iter().find(|b| b.url == url).map(|b| books::label(b, books)).unwrap_or_default();
        match self {
            Self::Card(c, also) => {
                if own && books.len() > 1 {
                    add(name_of(&c.book));
                }
                if !also.is_empty() {
                    add(format!("also in {}", also.iter().map(|u| name_of(u)).collect::<Vec<_>>().join(", ")));
                }
            }
            Self::Saved(_) if own && !books.is_empty() => add("on this computer".to_owned()),
            Self::Saved(_) => {}
        }
        (self.title(), second)
    }

    fn matches(&self, query: &str) -> bool {
        match self {
            Self::Card(c, _) => beavermail_store::Contact { name: c.name.clone(), address: c.emails.join(" "), folded: beavermail_store::fold(&format!("{} {} {}", c.name, c.emails.join(" "), c.phones.join(" "))) }.matches(query),
            Self::Saved(c) => c.matches(query),
        }
    }
}

/// The parts of the app the rail at the window's left edge switches between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Mail,
    Contacts,
    Calendar,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum SettingsPage {
    #[default]
    Reading,
    Appearance,
    Writing,
    Spam,
    Encryption,
    Storage,
    Troubleshooting,
    /// One account's page, by serial.
    Account(u64),
}

impl SettingsPage {
    const ALL: [Self; 7] = [Self::Reading, Self::Appearance, Self::Writing, Self::Spam, Self::Encryption, Self::Storage, Self::Troubleshooting];

    fn title(self) -> &'static str {
        match self {
            Self::Reading => "Reading",
            Self::Appearance => "Appearance",
            Self::Writing => "Writing",
            Self::Spam => "Suspicious mail",
            Self::Storage => "Stored mail",
            Self::Encryption => "Encryption",
            Self::Troubleshooting => "Troubleshooting",
            Self::Account(_) => "Account",
        }
    }

    fn icon(self) -> Option<Icon> {
        match self {
            Self::Reading => Some(Icon::MailOpen),
            Self::Appearance => Some(Icon::Palette),
            Self::Writing => Some(Icon::Pencil),
            Self::Spam => Some(Icon::Spam),
            Self::Storage => Some(Icon::Lock),
            Self::Encryption => Some(Icon::Key),
            Self::Troubleshooting => Some(Icon::LifeBuoy),
            Self::Account(_) => None,
        }
    }

    fn about(self) -> &'static str {
        match self {
            Self::Reading => "Most unwanted mail and tracking arrives as HTML, so messages start as text.",
            Self::Appearance => "How the app looks.",
            Self::Writing => "How messages you write are composed and sent.",
            Self::Spam => "Mail from people you know is never held. Nothing moves on the server.",
            Self::Storage => "How downloaded messages are kept on this computer.",
            Self::Encryption => "Your keys, to read encrypted mail and to send it encrypted.",
            Self::Troubleshooting => "What to send when something is slow or does not work.",
            Self::Account(_) => "",
        }
    }
}

pub(crate) const FORM_WIDTH: f32 = 460.0;
const SETTINGS_WIDTH: f32 = 780.0;
const PALETTE_WIDTH: f32 = 560.0;
/// How far the toast sits above the window's bottom edge: clear of the status bar.
const TOAST_LIFT: f32 = 44.0;
const PALETTE_LIST_H: f32 = 360.0;
/// The settings sidebar, page list and accounts.
const SETTINGS_NAV: f32 = 220.0;
/// Text field width in the account form: the form minus its label column and spacing.
pub(crate) const FIELD_WIDTH: f32 = 310.0;

fn form_grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 12.0]).min_col_width(84.0).show(ui, add);
}

fn field(text: &mut String) -> egui::TextEdit<'_> {
    sized_field(text, FIELD_WIDTH)
}

/// A text field as tall as every other control; egui's default is a cramped 20px.
fn sized_field(text: &mut String, width: f32) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text)
        .desired_width(width)
        .min_size(vec2(width, theme::CONTROL_H))
        .margin(egui::Margin::symmetric(12, 0))
        .vertical_align(Align::Center)
}

/// The message list's scroll area, moved just enough to show row `reveal`, from what it showed last
/// frame: (offset, height).
fn list_area(reveal: Option<usize>, (offset, height): (f32, f32)) -> egui::ScrollArea {
    let area = egui::ScrollArea::vertical().auto_shrink([false, false]);
    match reveal.and_then(|row| reveal_offset(row, offset, height)) {
        Some(to) => area.vertical_scroll_offset(to),
        None => area,
    }
}

/// The scroll offset that shows row `row` with the least movement, or `None` when it is in view already.
fn reveal_offset(row: usize, offset: f32, height: f32) -> Option<f32> {
    let top = row as f32 * views::MESSAGE_ROW_H;
    if top < offset {
        Some(top)
    } else if top + views::MESSAGE_ROW_H > offset + height {
        Some(top + views::MESSAGE_ROW_H - height)
    } else {
        None
    }
}

/// Marks the pane the keys act on after F6.
/// Where a folder opened in the sidebar is remembered, in [`Settings::folded`] beside the accounts:
/// folders start folded, the user opens those they want.
fn folder_open_key(account: &str, folder: &str) -> String {
    format!("open:{account}:{folder}")
}

/// A folder row of an account in the sidebar, drawn in from the left: a margin for the arrow of a folder
/// with others inside (one level in from the account's own), then a step per level of the folders it is in.
fn indented(ui: &mut egui::Ui, depth: usize, row: impl FnOnce(&mut egui::Ui) -> egui::Response) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(10.0 + 16.0 * depth as f32); // with the row's spacing, 18 points for the arrow
        row(ui)
    })
    .inner
}

fn pane_ring(ui: &egui::Ui, t: &theme::Tokens, rect: egui::Rect) {
    ui.painter().rect_stroke(rect.shrink(1.0), egui::CornerRadius::ZERO, egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(24.0);
    ui.label(RichText::new(text).size(theme::SUBTITLE).family(theme::bold()));
    ui.add_space(2.0);
}

/// One account's page in Settings. `waiting`: a sign-in for it is open in the browser.
fn account_page(ui: &mut egui::Ui, t: &theme::Tokens, form: &mut NewAccount, waiting: bool) -> Option<PageAction> {
    let mut action = None;
    let hint = |ui: &mut egui::Ui, text: &str| ui.label(RichText::new(text).size(13.0).color(t.text_dim));
    form_grid(ui, "account_identity", |ui| {
        ui.label("Your name");
        ui.add(field(&mut form.name).hint_text("What recipients see beside your address"));
        ui.end_row();
        ui.label("Email");
        ui.add(field(&mut form.email).hint_text("Address mail is sent from"));
        ui.end_row();
    });

    section_title(ui, "Signature");
    ui.add(egui::TextEdit::multiline(&mut form.signature).id_salt("signature").desired_rows(4).desired_width(f32::INFINITY).margin(egui::Margin::same(12)));
    hint(ui, "Added under new messages, replies and forwards. Leave empty for none.");

    section_title(ui, "Sync");
    form_grid(ui, "account_sync", |ui| {
        ui.label("Keep offline");
        let label = |k: Keep| match k {
            Keep::All => "Everything",
            Keep::Year => "Last 12 months",
            Keep::Month => "Last 30 days",
            Keep::Opened => "Only messages I open",
        };
        egui::ComboBox::from_id_salt("keep").width(FIELD_WIDTH).selected_text(label(form.keep)).show_ui(ui, |ui| {
            for keep in [Keep::All, Keep::Year, Keep::Month, Keep::Opened] {
                ui.selectable_value(&mut form.keep, keep, label(keep));
            }
        });
        ui.end_row();
    });
    views::switch_row(ui, t, &mut form.save_sent, "Keep a copy of sent messages in Sent", "Turn off for providers that already do it, such as Gmail and Microsoft 365.");

    section_title(ui, "Connection");
    match form.oauth {
        Some(provider) if waiting => {
            hint(ui, &format!("Finish signing in to {} in your browser.", provider.name()));
            ui.add_space(4.0);
            if ui.button("Cancel sign-in").clicked() {
                action = Some(PageAction::CancelSignIn);
            }
        }
        Some(provider) => {
            hint(ui, &format!("Signed in with {} as {}. Sign in again if the account stops connecting.", provider.name(), form.user));
            ui.add_space(4.0);
            if ui.button("Sign in again…").clicked() {
                action = Some(PageAction::SignIn(provider));
            }
        }
        None => {
            hint(ui, "Saving a change here reconnects the account. A different user or server downloads its mail again.");
            ui.add_space(4.0);
            form_grid(ui, "account_login", |ui| login_rows(ui, form));
            server_sections(ui, t, form);
        }
    }
    if !waiting {
        ui.add_space(8.0);
        if check_rows(ui, t, form) {
            action = Some(PageAction::Test);
        }
    }

    section_title(ui, "Remove account");
    hint(ui, "Deletes the mail downloaded to this computer and the saved password. Nothing changes on the server.");
    ui.add_space(4.0);
    let remove = egui::Button::new(RichText::new("Remove account…").color(t.danger)).stroke(egui::Stroke::new(1.0, t.danger));
    if ui.add(remove).clicked() {
        action = Some(PageAction::Remove);
    }
    ui.add_space(8.0);
    action
}

/// Everyone in the Contacts section: the address books' cards, and the contacts kept only here (those
/// no card has), by name.
fn people(archive: &Store) -> Vec<Person> {
    let cards = archive.address_book().unwrap_or_default();
    // Each address, with the address books that have it: someone in two of them shows in both, marked.
    let mut held: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for card in &cards {
        for email in &card.emails {
            let books = held.entry(email.as_str()).or_default();
            if !books.contains(&card.book.as_str()) {
                books.push(&card.book);
            }
        }
    }
    let also = |card: &beavermail_store::StoredCard| -> Vec<String> {
        let mut others: Vec<String> = card.emails.iter().flat_map(|e| held[e.as_str()].iter()).filter(|b| **b != card.book).map(|b| b.to_string()).collect();
        others.dedup();
        others
    };
    let mut all: Vec<Person> = cards.iter().map(|c| Person::Card(c.clone(), also(c))).collect();
    all.extend(archive.saved_contacts().unwrap_or_default().into_iter().filter(|c| !held.contains_key(c.address.as_str())).map(Person::Saved));
    all.sort_by_key(|p| p.title().to_lowercase());
    all
}

/// A key file read as an OpenPGP key, or (`certificate`) an S/MIME `.p12`, unlocked with `password`.
fn import_file(file: &[u8], certificate: bool, password: Option<&str>) -> Result<Vec<u8>, beavermail_crypto::ImportError> {
    if certificate { beavermail_crypto::import_certificate(file, password) } else { beavermail_crypto::import(file, password) }
}

/// Why a key file could not be used, for the user.
fn import_error(e: &beavermail_crypto::ImportError) -> &'static str {
    match e {
        beavermail_crypto::ImportError::NotASecretKey => "This file has no secret key: it may be someone's public key, or not a key at all.",
        beavermail_crypto::ImportError::NeedsPassword => "This key is protected: it needs its password.",
        beavermail_crypto::ImportError::WrongPassword => "That password does not open this key.",
    }
}

/// The mark for what was typed in "Add a trusted sender": an address, or `@domain` for its whole
/// organisation (`@mail.shop.example` trusts `@shop.example`, as marks from messages do); `None` if it
/// is neither.
fn trusted_key(text: &str) -> Option<String> {
    let text = text.trim().to_lowercase();
    let (user, domain) = text.split_once('@')?;
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') || domain.contains(char::is_whitespace) || user.contains(char::is_whitespace) {
        return None;
    }
    Some(if user.is_empty() { format!("@{}", sender::registrable(domain)) } else { text })
}

/// "Test connection" and, once it ran for the form as it is, what each server answered.
fn check_rows(ui: &mut egui::Ui, t: &theme::Tokens, form: &NewAccount) -> bool {
    let testing = form.current_check().is_some_and(|c| c.answers.is_none());
    let pressed = ui.add_enabled(form.ready() && !testing, egui::Button::new("Test connection")).on_hover_text("Sign in to both servers now, to find a mistake before saving").clicked();
    match form.current_check().map(|c| &c.answers) {
        Some(None) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Signing in to both servers…").color(t.text_dim));
            });
        }
        Some(Some((imap, smtp))) => {
            for (what, answer) in [("Incoming mail (IMAP)", imap), ("Outgoing mail (SMTP)", smtp)] {
                let (text, color) = match answer {
                    Ok(()) => (format!("{what}: signed in"), t.accent),
                    Err(why) => (format!("{what}: {why}"), t.danger),
                };
                ui.add(egui::Label::new(RichText::new(text).size(13.0).color(color)).wrap());
            }
        }
        None => {}
    }
    pressed
}

/// User and password, as rows of the caller's form grid.
fn login_rows(ui: &mut egui::Ui, form: &mut NewAccount) {
    ui.label("User");
    ui.add(field(&mut form.user).hint_text("Login name, often the same address"));
    ui.end_row();
    ui.label("Password");
    let hint = if form.editing.is_some() { "Leave empty to keep the current one" } else { "" };
    ui.add(field(&mut form.password).password(true).hint_text(hint));
    ui.end_row();
}

/// The incoming and outgoing servers, each under its own title.
fn server_sections(ui: &mut egui::Ui, t: &theme::Tokens, form: &mut NewAccount) {
    for (title, id, host, port, tls, hint) in [
        ("Incoming mail (IMAP)", "imap", &mut form.imap_host, &mut form.imap_port, &mut form.imap_tls, "imap.example.com"),
        ("Outgoing mail (SMTP)", "smtp", &mut form.smtp_host, &mut form.smtp_port, &mut form.smtp_tls, "Same as IMAP"),
    ] {
        ui.add_space(8.0);
        ui.label(RichText::new(title).family(theme::bold()));
        form_grid(ui, &format!("{id}_form"), |ui| {
            ui.label("Server");
            ui.add(field(host).hint_text(hint));
            ui.end_row();
            ui.label("Port");
            ui.horizontal(|ui| {
                ui.add(sized_field(port, 72.0));
                tls_picker(ui, &format!("{id}_tls"), tls);
            });
            ui.end_row();
        });
    }
    smtp_login(ui, t, form);
}

/// The outgoing server's own login, under its section, when it is not the incoming one's.
fn smtp_login(ui: &mut egui::Ui, t: &theme::Tokens, form: &mut NewAccount) {
    if form.oauth.is_some() {
        return;
    }
    ui.add_space(4.0);
    views::switch_row(ui, t, &mut form.smtp_own, "Different user and password for sending", "For servers that send with a login of their own.");
    if form.smtp_own {
        ui.add_space(4.0);
        form_grid(ui, "smtp_login", |ui| {
            ui.label("User");
            ui.add(field(&mut form.smtp_user).hint_text("Login for the outgoing server"));
            ui.end_row();
            ui.label("Password");
            let hint = if form.editing.is_some() { "Leave empty to keep the current one" } else { "" };
            ui.add(field(&mut form.smtp_password).password(true).hint_text(hint));
            ui.end_row();
        });
    }
}

fn tls_picker(ui: &mut egui::Ui, id: &str, tls: &mut Tls) {
    let label = |t: Tls| match t {
        Tls::StartTls => "STARTTLS",
        Tls::Tls => "TLS",
        Tls::None => "None (insecure)",
    };
    egui::ComboBox::from_id_salt(id).selected_text(label(*tls)).show_ui(ui, |ui| {
        for t in [Tls::StartTls, Tls::Tls, Tls::None] {
            ui.selectable_value(tls, t, label(t));
        }
    });
}

struct BeaverApp {
    runtime: Runtime,
    ctx: egui::Context,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    accounts: Vec<AccountView>,
    next_serial: u64,
    /// Selected folder: account serial and index in that account's mailboxes.
    selected: Option<(u64, usize)>,
    messages: Vec<MessageSummary>,
    message: Option<MessageId>,
    body: Option<OpenBody>,
    error: Option<String>,
    form: Option<NewAccount>,
    /// A message waiting for the user to confirm erasing it for good.
    /// Messages to erase for good from the Trash, until the user confirms; empty when not asking.
    confirm_erase: Vec<MessageId>,
    /// Messages picked in the list with Ctrl+click, Shift+click or Ctrl+A, the first being where a
    /// Shift+click range starts; commands apply to all of them once there are two or more.
    picked: Vec<MessageId>,
    /// "Move all to Spam" was pressed in the suspicious view; waiting for the user to confirm.
    confirm_spam: bool,
    /// The Spam or Trash folder (account, index) the user asked to empty, until they confirm.
    confirm_empty: Vec<(u64, usize)>,
    /// Text of the search box; filters the loaded messages by sender and subject.
    query: String,
    /// The query the results below belong to, which search they are, and what was found: (account, hit).
    searched: String,
    search_seq: u64,
    hits_seq: u64,
    hits: Vec<(u64, Hit)>,
    focus_search: bool,
    compose: Option<compose::Compose>,
    /// Drafts on this computer for the account being shown (filled while a Drafts folder is open).
    drafts: Vec<drafts::DraftData>,
    /// Why the last attempt to keep a draft on the server failed; shown in the status bar until one works.
    draft_error: Option<String>,
    /// A compose window with text in it is being closed; waiting for the user to confirm.
    confirm_discard: bool,
    /// That question was asked because the app is closing: the answer closes it, unless it is Keep editing.
    quitting: bool,
    /// Short confirmation ("Message sent") and when it was shown.
    notice: Option<(String, std::time::Instant)>,
    settings: Settings,
    /// The local mail archive every account reads from first.
    archive: Arc<Store>,
    /// The encrypted message files.
    vault: Arc<Vault>,
    /// Syncs in flight; the list shows a spinner while it is not zero.
    syncing: u32,
    show_settings: bool,
    settings_page: SettingsPage,
    /// The account page being edited in Settings; nothing is saved until Save changes.
    account_edit: Option<NewAccount>,
    /// A sign-in with a provider, from Add account or an account's page, until it ends.
    signing_in: Option<SignIn>,
    /// What the window shows, as picked in the rail at its left edge.
    section: Section,
    /// Settings > Encryption's dialogs.
    key_dialogs: encryption::Dialogs,
    /// The user's own keys and certificates, by their place in the vault, read when first asked for.
    key_files: std::collections::HashMap<std::path::PathBuf, Option<Vec<u8>>>,
    /// Whether the open message can be encrypted, for which sender and recipients (a hash of them).
    encryption_checked: Option<(u64, Result<(), String>)>,
    /// A person's card, open over everything else.
    card: Option<card::Card>,
    /// The message whose text could not be loaded, and why: shown in the reader in place of a spinner.
    body_failed: Option<(MessageId, String)>,
    /// The card open in the Contacts section.
    contact_pane: Option<card::Card>,
    /// The address books (by URL) whose sync runs now, and when all were last synced.
    contacts_syncing: std::collections::HashSet<String>,
    /// The "Add address book" dialog, and the address book waiting for "Stop syncing" to be confirmed.
    add_book: Option<books::AddBook>,
    confirm_book_remove: Option<String>,
    contacts_synced: Option<std::time::Instant>,
    /// The saved contacts for the Contacts section, read when it is drawn after a change.
    address_book: Option<Vec<Person>>,
    /// The address book whose contacts the Contacts section shows (by URL); `None` for all of them.
    contacts_shown: Option<String>,
    /// The photos of the cards shown in the Contacts list (by card id), read as their rows come into
    /// view; read again with the list.
    contact_photos: std::collections::HashMap<i64, Option<egui::TextureHandle>>,
    /// The address-book card waiting for the user to confirm deleting it everywhere.
    confirm_card_delete: Option<i64>,
    /// After an address book's first sync: (its URL, the contacts kept only here, each ticked or not).
    /// What is typed in the Contacts section's search, and the "New contact" dialog: (address, name).
    contact_query: String,
    new_contact: Option<(String, String)>,
    new_folder: Option<NewFolder>,
    /// Delete folder… asked for (account, folder name); waiting for the user to confirm. With why the
    /// server refused it in the trash, when it did: then it can only be deleted for good.
    confirm_folder_delete: Option<(u64, String, Option<String>)>,
    /// Everyone written to, to suggest in a message's address fields; read again for each new message.
    contacts: Arc<Vec<beavermail_store::Contact>>,
    /// Where the user asked to go from an account page with unsaved changes (`None`: close Settings),
    /// until they choose to discard them.
    settings_leave: Option<Option<SettingsPage>>,
    /// The account the user asked to remove, until they confirm.
    confirm_remove: Option<u64>,
    /// Unsubscribe was pressed (who, how), until the user confirms.
    confirm_unsubscribe: Option<(String, unsubscribe::Method)>,
    /// Block sender was pressed on this message, until the user confirms.
    confirm_block: Option<MessageId>,
    /// The source tab shown, or the last one.
    source_view: Option<SourceView>,
    /// The command palette, while open.
    palette: Option<Palette>,
    /// The last Archive or Move to Trash, until it goes to the server.
    pending: Option<Pending>,
    /// Outbox messages being sent now: (account, the draft).
    outbox_sending: std::collections::HashMap<String, (u64, drafts::DraftData)>,
    /// Messages waiting in the Outbox to go as soon as they can, every account together.
    outbox: usize,
    /// Messages to send later, every account's, with the account they go from.
    scheduled: Vec<(u64, drafts::DraftData)>,
    /// The list shows the scheduled messages instead of a folder.
    show_scheduled: bool,
    /// The user chose to work offline: nothing goes to the network until they go online again. Not
    /// remembered, so a restart is online and the app is never offline without the user knowing.
    work_offline: bool,
    /// Changes made while the server could not be reached, made when it answers: (account, folder
    /// index, message, action). Kept in the vault too (see [`Self::save_offline`]), so closing the app
    /// does not drop them.
    offline_changes: Vec<(u64, usize, MessageId, Action)>,
    /// Changes sent to the server and not answered yet, as in `offline_changes`; closing waits for them.
    in_flight: Vec<(u64, usize, MessageId, Action)>,
    /// The window was closed with changes in flight: it closes once they are answered.
    closing: bool,
    /// The pane F6 moved the keys to; `None` until then, and again after a click in the list.
    pane: Option<Pane>,
    /// A scroll of the reader asked for from the keyboard, done when it is next drawn.
    reader_scroll: Option<Scroll>,
    /// The Inbox of every account is shown together (or their Spam, see `unified_role`); `selected` is
    /// then `None`.
    unified: bool,
    /// Which folder of every account `unified` shows: the Inbox or Spam.
    unified_role: Role,
    /// The account each listed message (and each Sent one) comes from.
    origin: std::collections::HashMap<MessageId, u64>,
    /// The Sent folder of the account shown, or of every account (empty when Sent itself is shown).
    sent: Vec<MessageSummary>,
    /// The conversations of the folder shown, worked out again when it changes.
    conversations: threads::Conversations,
    /// The reader shows every message of the open conversation, not only the latest few.
    whole_conversation: bool,
    /// The Calendar section: its view, the day it is around, and the month on its left.
    calendar_view: calview::View,
    calendar_day: chrono::NaiveDate,
    calendar_month: chrono::NaiveDate,
    /// Occurrences worked out for a range, by (first day, days, the calendars shown, version), the
    /// latest few; the version moves on when events or calendars change.
    calendar_cache: Vec<(CalendarKey, Arc<Vec<calview::Item>>)>,
    calendar_version: u64,
    /// The event whose details are open.
    calendar_open: Option<calview::Item>,
    /// The week view scrolls to the morning when it next shows a week.
    calendar_morning: bool,
    add_calendar: Option<books::AddBook>,
    calendars_syncing: std::collections::HashSet<String>,
    /// Stop syncing was asked for this calendar (by URL), until confirmed.
    confirm_calendar_remove: Option<String>,
    /// The dialog to add or change an event, while open.
    event_form: Option<event_form::EventForm>,
    /// Asking whether to delete the event with this UID.
    confirm_event_delete: Option<String>,
    /// Other messages of the open conversation shown open in the reader, beside the tab's own
    /// (`message`), in the order they were opened.
    unfolded: Vec<Unfolded>,
    /// The tab's own message is closed in its conversation.
    folded_anchor: bool,
    /// The message last asked for in the reader, so a load it no longer wants stops early.
    wanted: Arc<std::sync::atomic::AtomicU64>,
    /// A list row to bring into view after the keyboard moved to it.
    list_reveal: Option<usize>,
    /// What the list showed last frame: (scroll offset, height).
    list_view: (f32, f32),
    /// Asking whether to forget the senders and messages marked as spam or not spam.
    confirm_forget_marks: bool,
    /// What is typed in "Add a trusted sender".
    trust_input: String,
    /// What is typed to find a sender marked as spam, in Settings.
    spam_query: String,
    /// The compose field last typed in: Esc takes the focus away, and Keep editing gives it back.
    compose_focus: Option<egui::Id>,
    refocus: u8,
    /// The user paused the background downloads.
    downloads_paused: bool,
    /// An attachment that runs code, waiting for the user to say yes to opening it.
    confirm_open: Option<(MessageId, u32, String)>,
    /// Attachments ticked in the reader, to save together.
    attach_selected: std::collections::BTreeSet<(u64, u32)>,
    /// Where private copies of opened attachments go; emptied at start and exit.
    open_dir: std::path::PathBuf,
    /// Settings and prompt state for the stored-mail protection.
    security: security::Form,
    /// A mode change rewriting files in the background.
    vault_job: Option<Arc<security::Job>>,
    /// The unlock prompt was dismissed for this run.
    unlock_skipped: bool,
    /// An unlock or reset is running.
    unlocking: bool,
    /// Display mode chosen for the open message only; forgotten when another is opened.
    view_modes: std::collections::HashMap<MessageId, DisplayMode>,
    /// Open messages: the reading pane's and those in tabs; the one shown is `message`.
    tabs: Vec<tabs::Tab>,
    /// A message tab fills the window, rather than the mail (folders, list and reading pane).
    in_tab: bool,
    /// Where the checks put each message of the Inbox being shown; empty until they have run.
    screening: std::collections::HashMap<MessageId, screen::Screened>,
    /// The selected account's suspicious mail is shown instead of its Inbox (in All inboxes, every account's).
    show_suspicious: bool,
    /// How much mail each account (by serial) held as suspicious when its Inbox was last checked, so
    /// the count shows while another folder is open.
    /// (held, of which unread): the unread ones are taken off the Inbox's unread count.
    held_counts: std::collections::HashMap<u64, (u32, u32)>,
    /// The spam checks are running over the Inbox being shown.
    checking: bool,
}

impl BeaverApp {
    fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let (vault, vault_error) = match store::data_dir().and_then(|dir| Vault::open(&dir, Box::new(SystemStore)).map_err(|e| e.to_string())) {
            Ok(vault) => (vault, None),
            // Never a plain-text fallback: without the key, messages are kept in memory only.
            Err(e) => (Vault::in_memory(), Some(format!("Downloaded messages cannot be stored safely: {e}"))),
        };
        // The index is encrypted with a key derived from the vault's, so it follows the same setting.
        let opened = store::archive_path().and_then(|path| {
            let store = if vault.is_locked() { Store::locked(&path) } else { Store::open(&path, vault.derived_key("index").as_deref()) };
            store.map_err(|e| e.to_string())
        });
        let (archive, archive_error) = match opened {
            Ok(archive) => (archive, None),
            // Without a usable file the app still works, but nothing is kept between runs.
            Err(e) => (Store::in_memory().expect("in-memory archive"), Some(format!("The local mail archive is unavailable: {e}"))),
        };
        let mut app = Self {
            runtime: Runtime::new().expect("tokio runtime"),
            ctx,
            tx,
            rx,
            accounts: Vec::new(),
            next_serial: 0,
            selected: None,
            messages: Vec::new(),
            message: None,
            body: None,
            error: None,
            form: None,
            confirm_erase: Vec::new(),
            picked: Vec::new(),
            confirm_spam: false,
            confirm_empty: Vec::new(),
            query: String::new(),
            searched: String::new(),
            search_seq: 0,
            hits_seq: 0,
            hits: Vec::new(),
            focus_search: false,
            compose: None,
            drafts: Vec::new(),
            draft_error: None,
            confirm_discard: false,
            quitting: false,
            notice: None,
            settings: Settings::default(),
            archive: Arc::new(archive),
            vault: Arc::new(vault),
            syncing: 0,
            show_settings: false,
            settings_page: SettingsPage::default(),
            account_edit: None,
            signing_in: None,
            contacts: Default::default(),
            section: Section::Mail,
            key_dialogs: Default::default(),
            key_files: Default::default(),
            encryption_checked: None,
            card: None,
            contact_pane: None,
            add_book: None,
            confirm_book_remove: None,
            contacts_syncing: Default::default(),
            contacts_synced: None,
            body_failed: None,
            address_book: None,
            contacts_shown: None,
            contact_photos: std::collections::HashMap::new(),
            confirm_card_delete: None,
            contact_query: String::new(),
            new_contact: None,
            new_folder: None,
            confirm_folder_delete: None,
            settings_leave: None,
            confirm_remove: None,
            confirm_unsubscribe: None,
            confirm_block: None,
            source_view: None,
            palette: None,
            pending: None,
            outbox_sending: std::collections::HashMap::new(),
            outbox: 0,
            scheduled: Vec::new(),
            show_scheduled: false,
            offline_changes: Vec::new(),
            in_flight: Vec::new(),
            closing: false,
            work_offline: false,
            pane: None,
            reader_scroll: None,
            list_reveal: None,
            wanted: Arc::default(),
            unified: false,
            unified_role: Role::Inbox,
            origin: std::collections::HashMap::new(),
            sent: Vec::new(),
            conversations: threads::Conversations::default(),
            whole_conversation: false,
            calendar_view: calview::View::default(),
            calendar_day: chrono::Local::now().date_naive(),
            calendar_month: chrono::Local::now().date_naive().with_day(1).unwrap_or_default(),
            calendar_cache: Vec::new(),
            calendar_version: 0,
            calendar_open: None,
            calendar_morning: true,
            add_calendar: None,
            calendars_syncing: Default::default(),
            confirm_calendar_remove: None,
            event_form: None,
            confirm_event_delete: None,
            unfolded: Vec::new(),
            folded_anchor: false,
            list_view: (0.0, 0.0),
            confirm_forget_marks: false,
            trust_input: String::new(),
            spam_query: String::new(),
            compose_focus: None,
            refocus: 0,
            downloads_paused: false,
            confirm_open: None,
            attach_selected: std::collections::BTreeSet::new(),
            open_dir: store::data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("open"),
            security: security::Form::default(),
            vault_job: None,
            unlock_skipped: false,
            unlocking: false,
            view_modes: std::collections::HashMap::new(),
            tabs: Vec::new(),
            in_tab: false,
            screening: std::collections::HashMap::new(),
            show_suspicious: false,
            held_counts: Default::default(),
            checking: false,
        };
        app.error = archive_error.or(vault_error);
        let _ = std::fs::remove_dir_all(&app.open_dir); // copies of attachments from an earlier run
        match store::load_settings() {
            Ok(settings) => app.settings = settings,
            Err(e) => app.error = Some(e),
        }
        if app.settings.diagnostics {
            diag::set(store::data_dir().ok().as_deref());
        }
        match store::load() {
            Ok(accounts) => accounts.into_iter().for_each(|a| {
                app.add_view(a);
            }),
            Err(e) => app.error = Some(e),
        }
        app.count_outbox(); // what waited from the last session shows at once
        app.load_offline();
        app
    }

    /// Keeps the changes waiting for the server in the vault, encrypted like the mail, so they are
    /// made after a restart too. By account and message, which outlive a run; the folder is known again
    /// from the message.
    fn save_offline(&mut self) {
        let queued: Vec<(String, u64, Action)> = self.offline_changes.iter().filter_map(|(serial, _, id, action)| Some((self.view(*serial)?.account.key(), id.0, action.clone()))).collect();
        let path = std::path::Path::new(OFFLINE_CHANGES);
        let saved = if queued.is_empty() { self.vault.remove(path) } else { serde_json::to_vec(&queued).map_err(|e| MailError::Backend(e.to_string())).and_then(|json| self.vault.write(path, &json)) };
        if let Err(e) = saved {
            self.error = Some(format!("Changes waiting for the server may be lost when the app closes: {e}"));
        }
    }

    /// The changes a previous run left waiting for the server; they go once it answers. Read again when
    /// the stored mail is unlocked, as a locked vault cannot be read at start.
    fn load_offline(&mut self) {
        if !self.offline_changes.is_empty() {
            return;
        }
        let Some(json) = self.vault.read(std::path::Path::new(OFFLINE_CHANGES)).ok().flatten() else { return };
        let queued: Vec<(String, u64, Action)> = serde_json::from_slice(&json).unwrap_or_default();
        // A removed account's changes are dropped with it.
        self.offline_changes = queued.into_iter().filter_map(|(key, id, action)| Some((self.accounts.iter().find(|v| v.account.key() == key)?.serial, usize::MAX, MessageId(id), action))).collect();
    }

    /// Runs one mail call in the background and wakes the UI with its result.
    fn spawn(&self, fut: impl Future<Output = Event> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.runtime.spawn(async move {
            let _ = tx.send(fut.await); // the receiver is gone only when the app is closing
            ctx.request_repaint();
        });
    }

    /// Shows an account and starts loading its folders.
    fn add_view(&mut self, account: Account) -> u64 {
        self.insert_view(account, None)
    }

    /// Shows an account, at `position` or at the end, and returns its serial.
    fn insert_view(&mut self, account: Account, position: Option<usize>) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        let (source, sender, error) = match store::password(&account) {
            Ok(password) => {
                // A missing password of its own makes the outgoing server refuse, which sending then says.
                let smtp_password = if account.smtp_user.is_some() { store::smtp_password(&account).unwrap_or_default() } else { password.clone() };
                let (mut sender, mut source) = (SmtpSender::new(&account, smtp_password), ImapSource::new(&account, password, self.archive.clone(), self.vault.clone()));
                if let Some(provider) = account.oauth {
                    // The stored secret is the provider's refresh token; the servers get access tokens.
                    let token = store::oauth::token_source(provider, account.clone());
                    (sender, source) = (sender.with_token(token.clone()), source.with_token(token));
                }
                (Some(Arc::new(source)), Some(Arc::new(sender)), None)
            }
            Err(e) => (None, None, Some(e)),
        };
        if let (Some(source), Some(sender)) = (&source, &sender) {
            source.set_offline(self.work_offline);
            sender.set_offline(self.work_offline);
        }
        if let Some(source) = source.clone() {
            // The archive answers at once; the server's list follows and replaces it.
            let cached = source.clone();
            self.spawn(async move { Event::Mailboxes(serial, cached.cached_mailboxes().await, false) });
            self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await, true) });
        }
        let progress = Arc::new(Progress::default());
        progress.paused.store(self.downloads_paused, std::sync::atomic::Ordering::Relaxed);
        let view = AccountView { serial, account, source, sender, mailboxes: Vec::new(), error, offline: None, progress, watch_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)), watching: false, retrying: false, retry_wait: RETRY_FIRST };
        match position {
            Some(at) => self.accounts.insert(at.min(self.accounts.len()), view),
            None => self.accounts.push(view),
        }
        serial
    }

    fn view(&self, serial: u64) -> Option<&AccountView> {
        self.accounts.iter().find(|v| v.serial == serial)
    }

    fn select_mailbox(&mut self, serial: u64, index: usize) {
        let Some(view) = self.view(serial) else { return };
        if view.source.is_none() || view.mailboxes.get(index).is_none() {
            return;
        }
        let (key, name, delimiter) = (view.account.key(), &view.mailboxes[index].name, view.delimiter());
        // A folder opened from the palette or a link shows in the sidebar: the folders it is inside unfold.
        let outer: Vec<String> = view.mailboxes.iter().filter(|m| views::is_inside(name, &m.name, delimiter)).map(|m| folder_open_key(&key, &m.name)).collect();
        self.send_pending(); // Undo puts mail back in the list it left
        let before = self.settings.folded.len();
        self.settings.folded.extend(outer);
        if self.settings.folded.len() != before {
            self.save_settings();
        }
        self.selected = Some((serial, index));
        self.unified = false;
        self.searched.clear(); // a search runs again over this folder
        self.show_scheduled = false;
        self.refresh_drafts();
        // The reading pane belonged to the list being left; tabs stay.
        self.tabs.retain(|t| t.pinned);
        self.in_tab = false;
        self.picked.clear();
        self.message = None;
        self.body = None;
        self.messages.clear();
        self.origin.clear();
        self.sent.clear();
        self.screening.clear();
        self.checking = false;
        self.load_folder(serial, index);
    }

    /// Shows the Inbox of every account together, newest first.
    fn show_all_inboxes(&mut self) {
        self.show_all(Role::Inbox);
    }

    /// Shows the folder of every account with `role` (the Inbox or Spam) together, newest first.
    fn show_all(&mut self, role: Role) {
        self.send_pending();
        self.selected = None;
        (self.unified, self.unified_role) = (true, role);
        self.searched.clear(); // a search runs again over what is shown now
        self.show_scheduled = false;
        self.drafts.clear();
        self.tabs.retain(|t| t.pinned);
        self.in_tab = false;
        self.picked.clear();
        self.message = None;
        self.body = None;
        self.messages.clear();
        self.origin.clear();
        self.sent.clear();
        self.screening.clear();
        self.checking = false;
        for (serial, index) in self.shown() {
            self.load_folder(serial, index);
        }
    }

    /// Shows the messages scheduled to send later, from every account, in the list's place.
    fn show_scheduled_list(&mut self) {
        self.send_pending();
        (self.selected, self.unified, self.show_scheduled, self.show_suspicious) = (None, false, true, false);
        self.tabs.retain(|t| t.pinned);
        self.in_tab = false;
        self.picked.clear();
        (self.message, self.body) = (None, None);
        self.messages.clear();
        self.origin.clear();
        self.sent.clear();
        self.screening.clear();
        self.count_outbox();
    }

    /// What was chosen on a scheduled message's row: opened to change it (which unschedules it, as
    /// any opened Outbox message), sent now, kept as a plain draft, or deleted.
    fn scheduled_action(&mut self, serial: u64, draft: drafts::DraftData, action: views::DraftAction) {
        let Some(key) = self.view(serial).map(|v| v.account.key()) else { return };
        let result = match action {
            views::DraftAction::Open => {
                if self.compose.is_none() {
                    self.start_compose(Some(compose::Compose::from_draft(serial, draft)));
                }
                Ok(())
            }
            views::DraftAction::SendNow => drafts::save(&self.vault, &key, &drafts::DraftData { send_at: None, ..draft }),
            views::DraftAction::Unschedule => {
                self.notice = Some(("Moved back to Drafts".into(), std::time::Instant::now()));
                drafts::save(&self.vault, &key, &drafts::DraftData { queued: false, send_at: None, ..draft })
            }
            views::DraftAction::Delete => self.forget_draft(serial, &key, &draft.id, true),
            views::DraftAction::None => return,
        };
        if let Err(e) = result {
            self.error = Some(format!("Could not change the scheduled message: {e}"));
        }
        self.count_outbox();
        self.refresh_drafts();
        if matches!(action, views::DraftAction::SendNow) {
            self.send_outbox(serial);
        }
    }

    /// Reads folder `index` of account `serial` from the archive, its Sent folder for conversations,
    /// and brings it up to date from the server.
    fn load_folder(&mut self, serial: u64, index: usize) {
        let Some(view) = self.view(serial) else { return };
        let (Some(source), Some(mailbox)) = (view.source.clone(), view.mailboxes.get(index)) else { return };
        let name = mailbox.name.clone();
        let sent_folder = view.mailboxes.iter().find(|m| m.role == Role::Sent && m.name != name).map(|m| m.name.clone());
        let local = source.clone();
        let folder = name.clone();
        self.spawn(async move { Event::Messages(serial, index, local.messages(&folder).await) });
        if let Some(sent) = sent_folder {
            let local = source.clone();
            self.spawn(async move { Event::SentList(serial, index, local.messages(&sent).await) });
        }
        self.start_sync(serial, index, source, name);
    }

    /// The folders whose messages the list shows, as (account, folder index): the one selected, or
    /// every account's Inbox.
    fn shown(&self) -> Vec<(u64, usize)> {
        if !self.unified {
            return self.selected.into_iter().collect();
        }
        self.accounts.iter().filter_map(|v| Some((v.serial, v.mailboxes.iter().position(|m| m.role == self.unified_role)?))).collect()
    }

    fn shows(&self, serial: u64, index: usize) -> bool {
        self.shown().contains(&(serial, index))
    }

    /// Starts watching an account's Inbox (IDLE, or polling) so new mail shows up by itself.
    fn start_watch(&mut self, serial: u64) {
        let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) else { return };
        let (Some(source), Some(inbox)) = (view.source.clone(), view.mailboxes.iter().find(|m| m.role == Role::Inbox).map(|m| m.name.clone())) else { return };
        if std::mem::replace(&mut view.watching, true) {
            return;
        }
        let (stop, tx, ctx) = (view.watch_stop.clone(), self.tx.clone(), self.ctx.clone());
        self.runtime.spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let mut wait = std::time::Duration::from_secs(15);
            let notify = || {
                let _ = tx.send(Event::Changed(serial));
                ctx.request_repaint();
            };
            while !stop.load(Relaxed) {
                if source.watch(&inbox, &notify, &stop).await.is_ok() {
                    break; // asked to stop
                }
                // The connection was lost (or never worked): try again, less and less often.
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(300));
            }
        });
    }

    /// Starts the background download for an account unless one is running.
    fn start_download(&mut self, serial: u64) {
        let Some(view) = self.view(serial) else { return };
        let (Some(source), keep, progress) = (view.source.clone(), view.account.keep, view.progress.clone()) else { return };
        if keep == Keep::Opened || progress.running.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        progress.total.store(0, std::sync::atomic::Ordering::Relaxed);
        progress.done.store(0, std::sync::atomic::Ordering::Relaxed);
        let ctx = self.ctx.clone();
        self.spawn(async move {
            // Wake the window now and then so the status bar moves while batches arrive.
            let ticker = tokio::spawn({
                let ctx = ctx.clone();
                async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        ctx.request_repaint();
                    }
                }
            });
            let result = source.download(keep, &progress).await;
            ticker.abort();
            progress.running.store(false, std::sync::atomic::Ordering::Relaxed);
            progress.set_activity("");
            Event::Downloaded(serial, result)
        });
    }

    /// Brings a folder of the archive up to date in the background.
    fn start_sync(&mut self, serial: u64, index: usize, source: Arc<ImapSource>, folder: String) {
        self.syncing += 1;
        self.spawn(async move { Event::Synced(serial, index, source.sync(&folder).await) });
    }

    /// Shows message `id` of the folder shown in the reading pane.
    fn select_message(&mut self, id: MessageId) {
        self.open_message(id, false);
    }

    /// Shows message `id`: in the reading pane, or `pin`, in its tab (opened if need be).
    fn open_message(&mut self, id: MessageId, pin: bool) {
        let listed = self.messages.iter().find(|m| m.id == id).and_then(|m| Some(tabs::Tab { serial: self.serial_of(id)?, message: m.clone(), role: self.role(), pinned: pin, source: false }));
        let tabbed = || self.tabs.iter().find(|t| t.message.id == id).map(|t| tabs::Tab { serial: t.serial, message: t.message.clone(), role: t.role, pinned: pin, source: false });
        if let Some(tab) = listed.or_else(tabbed) {
            self.open(tab);
        }
    }

    /// Shows `tab`'s message, from any folder: in the reading pane, or when pinned, in its tab.
    fn open(&mut self, mut tab: tabs::Tab) {
        self.picked.clear();
        // Without the reading pane there is nowhere else to read it.
        tab.pinned |= !self.settings.reading_pane;
        let (id, unread) = (tab.message.id, tab.message.unread);
        // Suspicious mail and spam are opened to judge them, not to read them.
        let judging = self.show_suspicious || tab.role == Role::Junk;
        let at = match tabs::slot(&self.tabs, id, tab.pinned) {
            tabs::Slot::Existing(at) => at,
            tabs::Slot::Replace(preview) => {
                self.tabs[preview] = tab;
                preview
            }
            tabs::Slot::Push => {
                self.tabs.push(tab);
                self.tabs.len() - 1
            }
        };
        // Also when it is the active one but its text is not there (only selected, or failed before).
        if self.active() != Some(at) || self.body.is_none() {
            self.activate(at);
        }
        // Its unread messages and the latest open around it, read in a row.
        if self.threaded() {
            let members: Vec<MessageId> = self.conversations.members(id).iter().copied().filter(|m| self.summary_of(*m).is_some()).collect();
            let last = members.last().copied();
            for m in members {
                if m != id && (Some(m) == last || self.summary_of(m).is_some_and(|s| s.unread)) {
                    self.unfold(m);
                }
            }
        }
        // Reading a conversation reads all of it, as its row shows it unread while any message is.
        let unread_here: Vec<MessageId> = self.conversation(id).into_iter().filter(|m| *m != id && self.summary_of(*m).is_some_and(|s| s.unread)).collect();
        if !judging && unread {
            self.act(id, Action::Unread(false));
        }
        if !judging {
            unread_here.into_iter().for_each(|m| self.act(m, Action::Unread(false)));
        }
    }

    /// Reads `message` of account `serial` and runs the checks on it, as a task to spawn; `None` when the
    /// account is not connected.
    fn read_message(&self, serial: u64, message: MessageSummary) -> Option<impl std::future::Future<Output = Result<Box<Prepared>, MailError>> + Send + 'static> {
        let view = self.view(serial)?;
        let (source, account) = (view.source.clone()?, view.account.clone());
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_owned()).collect();
        let (archive, settings) = (self.archive.clone(), self.settings.clone());
        Some(async move {
            let started = std::time::Instant::now();
            let body = source.body(message.id).await?;
            let read = started.elapsed();
            // The checks load what the account knows and train the spam model: seconds on a big mailbox, never on the UI thread.
            let prepared = tokio::task::spawn_blocking(move || {
                let checks = std::time::Instant::now();
                let findings = screen::Known::load(&archive, &account, &own, &settings)
                    .map(|known| screen::findings(&known, &message.from, &message.address, &body, &source.auth_server()))
                    .unwrap_or_default();
                let checks = checks.elapsed();
                let prepared = Box::new(Prepared::new(body, findings));
                log::debug!("message {}: read in {read:.0?}, checked in {checks:.0?}, ready in {:.0?}", message.id.0, started.elapsed());
                prepared
            });
            prepared.await.map_err(|e| MailError::Backend(e.to_string()))
        })
    }

    /// Opens message `id` of the open conversation in the reader, reading its text in the background.
    fn unfold(&mut self, id: MessageId) {
        if self.message == Some(id) {
            self.folded_anchor = false;
            return;
        }
        if self.unfolded.iter().any(|u| u.id == id) {
            return;
        }
        let Some(read) = self.serial_of(id).zip(self.summary_of(id).cloned()).and_then(|(serial, m)| self.read_message(serial, m)) else { return };
        self.unfolded.push(Unfolded { id, body: None });
        self.spawn(async move { Event::Unfolded(id, read.await) });
    }

    /// The message an answer goes to: the latest of the conversation of `id`, as in every threaded client.
    fn latest_of(&self, id: MessageId) -> MessageId {
        if !self.threaded() {
            return id;
        }
        self.conversations.members(id).iter().rev().copied().find(|m| self.summary_of(*m).is_some()).unwrap_or(id)
    }

    /// Shows tab `at` in the reader. Its message is loaded again: from this computer when stored, so
    /// switching is quick without keeping every open message in memory.
    fn activate(&mut self, at: usize) {
        let Some(tab) = self.tabs.get(at) else { return };
        let (serial, id, pinned) = (tab.serial, tab.message.id, tab.pinned);
        self.body_failed = None;
        if tab.source {
            let subject = tab.message.subject.clone();
            (self.in_tab, self.message, self.body) = (true, Some(id), None);
            self.source_view = Some(SourceView { id, subject, lines: None, size: 0, find: None });
            match self.view(serial).and_then(|v| v.source.clone()) {
                Some(source) => self.spawn(async move { Event::Source(id, source.source(id).await) }),
                None => self.source_view = Some(SourceView { id, subject: String::new(), lines: Some(Err("its account is not connected".into())), size: 0, find: None }),
            }
            return;
        }
        self.source_view = None;
        let Some(read) = self.read_message(serial, tab.message.clone()) else {
            // Never a reader waiting for nothing: an account without a connection says so.
            (self.in_tab, self.message, self.body) = (pinned, Some(id), None);
            self.body_failed = Some((id, "Its account is not connected. Check its password in Settings.".into()));
            return;
        };
        let wanted = self.wanted.clone();
        self.in_tab = pinned;
        self.whole_conversation &= self.message.is_some_and(|m| self.conversations.same(m, id));
        let conversations = &self.conversations;
        self.unfolded.retain(|u| u.id != id && conversations.same(u.id, id));
        self.message = Some(id);
        self.body = None;
        self.view_modes.retain(|m, _| conversations.same(*m, id));
        self.folded_anchor = false;
        self.attach_selected.clear();
        wanted.store(id.0, std::sync::atomic::Ordering::Relaxed);
        let asked = std::time::Instant::now();
        self.spawn(async move {
            // Paging through the list asks for many messages in a row: only the one it stops on is read.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            if wanted.load(std::sync::atomic::Ordering::Relaxed) != id.0 {
                return Event::Body(serial, id, Err(MailError::NotFound)); // dropped: another message is shown now
            }
            let body = read.await;
            log::debug!("message {}: shown {:.0?} after it was asked for", id.0, asked.elapsed());
            Event::Body(serial, id, body)
        });
    }

    /// Back to the mail, with the message that was in the reading pane.
    fn show_mail(&mut self) {
        match self.tabs.iter().position(|t| !t.pinned) {
            Some(pane) => self.activate(pane),
            None => {
                self.in_tab = false;
                self.body = None;
                // Without the reading pane the message stays selected, so the keys go on from it; with
                // the pane, nothing is shown beside the list.
                if self.settings.reading_pane {
                    self.message = None;
                }
            }
        }
    }

    /// Closes message tab `at`; when it was shown, the tab before it takes its place, or the mail.
    fn close_tab(&mut self, at: usize) {
        if !self.tabs.get(at).is_some_and(|t| t.pinned) {
            return;
        }
        let was_open = self.active() == Some(at);
        self.tabs.remove(at);
        if was_open {
            let pinned: Vec<usize> = (0..self.tabs.len()).filter(|i| self.tabs[*i].pinned).collect();
            match pinned.iter().rev().find(|i| **i < at).or(pinned.first()) {
                Some(&next) => self.activate(next),
                None => self.show_mail(),
            }
        }
    }

    /// Ctrl+Tab: the mail, then each message tab, round.
    fn cycle_tabs(&mut self, back: bool) {
        let mut order: Vec<Option<usize>> = vec![None];
        order.extend((0..self.tabs.len()).filter(|i| self.tabs[*i].pinned).map(Some));
        let now = order.iter().position(|o| *o == self.active().filter(|_| self.in_tab)).unwrap_or(0);
        let count = order.len();
        match order[if back { (now + count - 1) % count } else { (now + 1) % count }] {
            Some(at) => self.activate(at),
            None if self.in_tab => self.show_mail(),
            None => {}
        }
    }

    /// The index of the tab shown: a message tab, or the reading pane's.
    fn active(&self) -> Option<usize> {
        let id = self.message?;
        let source = self.showing_source();
        self.tabs.iter().position(|t| t.message.id == id && t.pinned == self.in_tab && t.source == source)
    }

    /// The tab shown.
    fn open_tab(&self) -> Option<&tabs::Tab> {
        self.active().map(|at| &self.tabs[at])
    }

    /// A message of the folder shown, of a tab or of the search results.
    fn summary_of(&self, id: MessageId) -> Option<&MessageSummary> {
        self.messages
            .iter()
            .find(|m| m.id == id)
            .or_else(|| self.tabs.iter().find(|t| t.message.id == id).map(|t| &t.message))
            .or_else(|| self.hits.iter().find(|(_, h)| h.message.id == id).map(|(_, h)| &h.message))
            .or_else(|| self.sent.iter().find(|m| m.id == id))
    }

    /// The list shows conversations: on in Settings, and not in search results, the suspicious mail,
    /// Drafts or Spam, where each message is judged or edited alone.
    fn threaded(&self) -> bool {
        self.settings.threads && !self.searching() && !self.show_suspicious && !matches!(self.role(), Role::Drafts | Role::Junk)
    }

    /// The messages of the folder shown that a command on `id` applies to: its whole conversation
    /// there when the list shows conversations, else `id` alone.
    fn conversation(&self, id: MessageId) -> Vec<MessageId> {
        if !self.threaded() || !self.messages.iter().any(|m| m.id == id) {
            return vec![id];
        }
        let ids: Vec<MessageId> = self.conversations.members(id).iter().copied().filter(|m| self.messages.iter().any(|x| x.id == *m)).collect();
        if ids.is_empty() { vec![id] } else { ids }
    }

    /// The account of message `id`: its tab's or search result's, which may not be the folder shown's.
    fn serial_of(&self, id: MessageId) -> Option<u64> {
        let tab = self.tabs.iter().find(|t| t.message.id == id).map(|t| t.serial);
        let hit = || self.hits.iter().find(|(_, h)| h.message.id == id).map(|(s, _)| *s);
        tab.or_else(hit).or_else(|| self.origin.get(&id).copied()).or(self.selected.map(|(s, _)| s))
    }

    /// What the folder of message `id` is for: its tab's, its search result's, or the folder shown.
    fn role_of(&self, id: MessageId) -> Role {
        let tab = self.tabs.iter().find(|t| t.message.id == id).map(|t| t.role);
        let hit = || self.hits.iter().find(|(_, h)| h.message.id == id).and_then(|(s, h)| self.view(*s)?.mailboxes.iter().find(|m| m.name == h.folder)).map(|m| m.role);
        tab.or_else(hit).unwrap_or_else(|| self.role())
    }

    /// The messages listed, in order: the search results while searching, else the folder's.
    fn listed(&self) -> Vec<MessageId> {
        if self.searching() { self.hits.iter().map(|(_, h)| h.message.id).collect() } else { self.visible().iter().map(|m| m.id).collect() }
    }

    /// Moves the selection to listed message `id`: shown in the reading pane, or without the pane only
    /// selected (Enter or a double click opens it in a tab).
    fn move_to(&mut self, id: MessageId) {
        if self.settings.reading_pane {
            self.show_listed(id);
        } else {
            // Only selected: nothing of it is loaded, and the text of the one before is not its text.
            self.message = Some(id);
            self.body = None;
            self.body_failed = None;
        }
    }

    /// Shows a listed message in the reading pane: a search result where it is, or one of the folder.
    fn show_listed(&mut self, id: MessageId) {
        match self.hits.iter().find(|(_, h)| h.message.id == id).cloned().filter(|_| self.searching()) {
            Some((serial, hit)) => self.open_hit(serial, &hit, false),
            None => self.select_message(id),
        }
    }

    /// The folder role the picked messages share, or `Other` when they come from different kinds of folder.
    fn picked_role(&self) -> Role {
        let mut roles = self.picked.iter().map(|id| self.role_of(*id));
        let first = roles.next().unwrap_or(Role::Other);
        if roles.all(|r| r == first) { first } else { Role::Other }
    }

    /// Applies a write to the list at once and sends it to the server; a failure reloads the folder.
    fn act(&mut self, id: MessageId, action: Action) {
        if matches!(action, Action::Reply | Action::ReplyAll | Action::Forward) {
            return;
        }
        // The message's account: its tab's, which may not be the folder shown, or the folder's.
        let Some(serial) = self.serial_of(id) else { return };
        let index = self.shown().into_iter().find(|(s, _)| *s == serial).map_or(usize::MAX, |(_, i)| i);
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        let pos = self.messages.iter().position(|m| m.id == id);
        // Archive, Move to Trash and to another folder of a listed message wait, so Undo can put it back.
        let undoable = matches!(action, Action::Archive | Action::Trash | Action::Move(_)) && self.role_of(id) != Role::Trash;
        let kept = pos.filter(|_| undoable).map(|p| (p, self.messages[p].clone()));
        if let Some(pos) = pos {
            let was_unread = self.messages[pos].unread;
            let unread_change = match action {
                Action::Unread(unread) => {
                    self.messages[pos].unread = unread;
                    i32::from(unread) - i32::from(was_unread)
                }
                Action::Pin(on) => {
                    self.messages[pos].pinned = on;
                    0
                }
                Action::Star(on) => {
                    self.messages[pos].starred = on;
                    0
                }
                Action::Label(n, on) => {
                    set_label(&mut self.messages[pos].labels, n, on);
                    0
                }
                _ => {
                    self.messages.remove(pos);
                    -i32::from(was_unread)
                }
            };
            if let Some(mailbox) = self.accounts.iter_mut().find(|v| v.serial == serial).and_then(|v| v.mailboxes.get_mut(index)) {
                mailbox.unread = mailbox.unread.saturating_add_signed(unread_change);
            }
        }
        if let Action::Unread(unread) = action {
            self.tabs.iter_mut().filter(|t| t.message.id == id).for_each(|t| t.message.unread = unread);
            self.hits.iter_mut().filter(|(_, h)| h.message.id == id).for_each(|(_, h)| h.message.unread = unread);
        }
        if let Action::Label(n, on) = action {
            self.tabs.iter_mut().filter(|t| t.message.id == id).for_each(|t| set_label(&mut t.message.labels, n, on));
            self.hits.iter_mut().filter(|(_, h)| h.message.id == id).for_each(|(_, h)| set_label(&mut h.message.labels, n, on));
        }
        if let Action::Star(on) = action {
            self.tabs.iter_mut().filter(|t| t.message.id == id).for_each(|t| t.message.starred = on);
            self.hits.iter_mut().filter(|(_, h)| h.message.id == id).for_each(|(_, h)| h.message.starred = on);
        }
        if let Action::Pin(on) = action {
            self.tabs.iter_mut().filter(|t| t.message.id == id).for_each(|t| t.message.pinned = on);
            self.hits.iter_mut().filter(|(_, h)| h.message.id == id).for_each(|(_, h)| h.message.pinned = on);
            // Pinned, it moves to the top (unpinned, back among the rest): the list follows the open one there.
            if let Some(open) = self.message.filter(|m| self.conversations.same(*m, id)) {
                self.list_reveal = self.listed().iter().position(|m| self.conversations.same(*m, open));
            }
        }
        match kept {
            Some((pos, message)) => {
                let delimiter = self.view(serial).and_then(AccountView::delimiter);
                let pending = self.pending.get_or_insert_with(|| Pending { text: String::new(), until: std::time::Instant::now(), items: Vec::new() });
                pending.items.push((serial, index, action.clone(), message, pos));
                pending.until = std::time::Instant::now() + std::time::Duration::from_secs(UNDO_SECS);
                let n = pending.items.len();
                let s = if n == 1 { "" } else { "s" };
                pending.text = match &action {
                    Action::Archive => format!("Archived {n} message{s}"),
                    Action::Move(folder) => format!("Moved {n} message{s} to {}", views::folder_title(folder, Role::Other, delimiter)),
                    _ => format!("Moved {n} message{s} to Trash"),
                };
            }
            None => self.change(source, serial, index, id, action.clone()),
        }
        if action.removes() {
            // The message left its folder: its tab goes, and triage goes on with the one that took
            // its place in the list.
            let was_open = self.message == Some(id);
            self.tabs.retain(|t| t.message.id != id);
            self.hits.retain(|(_, h)| h.message.id != id);
            // While searching, the list on screen is the results: nothing else is opened.
            if was_open && self.searching() {
                self.in_tab = false;
                self.message = None;
                self.body = None;
            } else if was_open {
                self.in_tab = false;
                self.message = None;
                self.body = None;
                if let Some(next) = pos.and_then(|p| self.messages.get(p).or(self.messages.last())).map(|m| m.id) {
                    self.move_to(next);
                }
            }
        }
    }

    /// The answer for a message sent from the Outbox: gone from it once sent, left in it while there is no
    /// connection, and back among the drafts, with the reason, when the server refused it.
    fn sent_from_outbox(&mut self, serial: u64, draft: drafts::DraftData, result: Result<(), MailError>) {
        let Some(view) = self.view(serial) else { return };
        let key = view.account.key();
        let subject = if draft.subject.trim().is_empty() { "(no subject)".to_owned() } else { draft.subject.trim().to_owned() };
        match result {
            Ok(()) => {
                // A copy may have been kept in the server's Drafts folder before it was sent.
                if let Err(e) = self.forget_draft(serial, &key, &draft.id, true) {
                    self.error = Some(format!("“{subject}” was sent, but it could not be taken out of the Outbox: {e}"));
                }
                self.notice = Some((format!("Sent “{subject}” from the Outbox"), std::time::Instant::now()));
            }
            Err(MailError::Offline(_)) => {}
            Err(e) => {
                let _ = drafts::save(&self.vault, &key, &drafts::DraftData { queued: false, ..draft });
                self.error = Some(format!("Could not send “{subject}”: {e}. It is back in Drafts."));
            }
        }
        self.count_outbox();
        self.refresh_drafts();
    }

    /// Works offline, or online again. Offline, the backends answer "offline" at once, so syncing,
    /// downloads and the Inbox watch stop, and sending and changes wait in their queues. Online, every
    /// account asks its server again, which also sends what waited.
    fn set_work_offline(&mut self, offline: bool) {
        self.work_offline = offline;
        for view in &mut self.accounts {
            if let (Some(source), Some(sender)) = (&view.source, &view.sender) {
                source.set_offline(offline);
                sender.set_offline(offline);
            }
            if offline {
                view.watch_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                view.watch_stop = Arc::new(std::sync::atomic::AtomicBool::new(false)); // for the next watch
                view.watching = false;
                view.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                view.offline = None;
                view.progress.cancel.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if !offline {
            // The server's answer starts downloads, the watch and the Outbox (see `Event::Mailboxes`).
            for (serial, source) in self.accounts.iter().filter_map(|v| Some((v.serial, v.source.clone()?))).collect::<Vec<_>>() {
                self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await, true) });
            }
            self.refresh();
        }
    }

    /// The server of account `serial` could not be reached: shown as a state, not an error.
    fn went_offline(&mut self, serial: u64, why: String) {
        if let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) {
            view.offline = Some(why);
        }
        self.retry_later(serial);
    }

    /// Offline (the server restarting, the network gone): its server is asked again on its own, less
    /// and less often, until it answers; then what waited is sent (see `Event::Mailboxes`).
    fn retry_later(&mut self, serial: u64) {
        let work_offline = self.work_offline;
        let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) else { return };
        let Some(source) = view.source.clone().filter(|_| !view.retrying && !work_offline) else { return };
        view.retrying = true;
        let wait = view.retry_wait;
        view.retry_wait = (wait * 2).min(RETRY_MOST);
        self.spawn(async move {
            tokio::time::sleep(wait).await;
            Event::Retried(serial, source.mailboxes().await)
        });
    }

    /// Makes `action` on the server, kept in `in_flight` until it answers.
    fn change(&mut self, source: Arc<ImapSource>, serial: u64, index: usize, id: MessageId, action: Action) {
        self.in_flight.push((serial, index, id, action.clone()));
        self.spawn(async move { Event::Done(serial, index, Some((id, action.clone())), perform(source, id, action).await) });
    }

    /// Sends the Archive or Move to Trash that was waiting for Undo.
    fn send_pending(&mut self) {
        let Some(pending) = self.pending.take() else { return };
        for (serial, index, action, message, _) in pending.items {
            if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
                self.change(source, serial, index, message.id, action);
            }
        }
    }

    /// Takes back the last Archive or Move to Trash: nothing reached the server, so the messages only
    /// return to the list, where they were, and the first of them opens again.
    fn undo(&mut self) {
        let Some(pending) = self.pending.take() else { return };
        let first = pending.items.first().map(|(_, _, _, m, _)| m.id);
        for (serial, index, _, message, pos) in pending.items.into_iter().rev() {
            if message.unread
                && let Some(mailbox) = self.accounts.iter_mut().find(|v| v.serial == serial).and_then(|v| v.mailboxes.get_mut(index))
            {
                mailbox.unread += 1;
            }
            let at = pos.min(self.messages.len());
            self.messages.insert(at, message);
        }
        if let Some(id) = first {
            self.move_to(id);
        }
    }

    /// "1,240 messages · 3 unread · 980 stored offline" for the folder being shown.
    fn folder_summary(&self) -> Option<String> {
        if self.unified && !self.searching() {
            let inboxes: Vec<&Mailbox> = self.shown().iter().filter_map(|(s, i)| self.view(*s)?.mailboxes.get(*i)).collect();
            let total = inboxes.iter().map(|m| m.total).sum::<u32>().max(self.messages.len() as u32);
            let unread: u32 = inboxes.iter().map(|m| m.unread).sum();
            let held = self.held();
            if self.show_suspicious {
                return Some(format!("Suspicious, every account  ·  {held} held  ·  nothing moved on the server"));
            }
            let label = if self.unified_role == Role::Junk { "Spam, every account" } else { "All inboxes" };
            let mut parts = vec![label.to_owned(), format!("{} message{}", views::thousands(total), if total == 1 { "" } else { "s" })];
            if unread > 0 {
                parts.push(format!("{} unread", views::thousands(unread)));
            }
            if held > 0 {
                parts.push(format!("{held} held as suspicious"));
            }
            if self.checking {
                parts.push("checking for spam…".to_owned());
            }
            return Some(parts.join("  ·  "));
        }
        let (serial, index) = self.selected?;
        let view = self.view(serial)?;
        let folder = view.mailboxes.get(index)?;
        if self.searching() {
            return Some(format!("{} result{}", views::thousands(self.hits.len() as u32), if self.hits.len() == 1 { "" } else { "s" }));
        }
        let total = folder.total.max(self.messages.len() as u32);
        let mut parts = vec![format!("{} message{}", views::thousands(total), if total == 1 { "" } else { "s" })];
        if folder.unread > 0 {
            parts.push(format!("{} unread", views::thousands(folder.unread)));
        }
        if folder.stored > 0 && view.account.keep != Keep::Opened {
            parts.push(format!("{} stored offline", views::thousands(folder.stored)));
        }
        let held = if folder.role == Role::Inbox { self.held() } else { 0 };
        let checking = self.checking && folder.role == Role::Inbox;
        if self.show_suspicious {
            parts = vec![format!("{held} held as suspicious  ·  nothing moved on the server")];
        } else if held > 0 {
            parts.push(format!("{held} held as suspicious"));
        }
        if checking {
            parts.push("checking for spam…".to_owned());
        }
        if (self.messages.len() as u32) < total {
            parts.push(if self.syncing > 0 { "loading older mail".to_owned() } else { format!("newest {} loaded", views::thousands(self.messages.len() as u32)) });
        }
        Some(parts.join("  ·  "))
    }

    /// The messages of the folder being shown, in list order (none while a search is showing).
    fn visible(&self) -> Vec<&MessageSummary> {
        if self.searching() {
            return Vec::new();
        }
        // In the Drafts folder, the copy of a draft that is also kept here is shown once, from here.
        let ours: Vec<String> = if self.role() == Role::Drafts {
            self.drafts.iter().map(|d| d.id.clone()).chain(self.compose.iter().map(|c| c.id.clone())).map(|id| beavermail_smtp::draft_message_id(&id)).collect()
        } else {
            Vec::new()
        };
        // The Inbox leaves out what is held as suspicious, once the checks have run; until then it shows
        // everything, and the suspicious view nothing.
        // shortcut: a kept verdict does not see a sender become known (or a rule change) until the Inbox
        // is checked in full from its menu; and new mail shows for a moment before its check, which
        // checking at sync, in the background, would avoid.
        // Per account: in All inboxes one Inbox may be checked before another.
        let screened: std::collections::HashSet<u64> = if self.holding() { self.screening.keys().filter_map(|id| self.origin.get(id).copied()).collect() } else { Default::default() };
        let checked = |m: &MessageSummary| self.origin.get(&m.id).is_some_and(|s| screened.contains(s));
        let want = if self.show_suspicious { screen::Place::Spam } else { screen::Place::Inbox };
        let threaded = self.threaded();
        let mut shown = std::collections::HashSet::new();
        let mut list = self.messages
            .iter()
            .filter(|m| !ours.contains(&m.message_id))
            .filter(|m| if checked(m) { self.screening.get(&m.id).is_some_and(|s| s.place == want) } else { !self.show_suspicious })
            // Newest first, so a conversation's row is its latest message here.
            .filter(|m| !threaded || self.conversations.number(m.id).is_none_or(|n| shown.insert(n)))
            .collect::<Vec<_>>();
        // Pinned first; then a conversation by its latest message, the user's replies from Sent included,
        // as threaded clients order them.
        let latest = self.conversation_dates();
        list.sort_by_key(|m| (!m.pinned, std::cmp::Reverse(latest.get(&m.id).map_or(m.timestamp, |l| l.0))));
        list
    }

    /// For each conversation's row message, when threaded: the time and date text of its latest
    /// message, from the folder or from Sent.
    fn conversation_dates(&self) -> std::collections::HashMap<MessageId, (i64, String)> {
        if !self.threaded() {
            return Default::default();
        }
        let all: std::collections::HashMap<MessageId, &MessageSummary> = self.messages.iter().chain(&self.sent).map(|m| (m.id, m)).collect();
        self.messages
            .iter()
            .filter_map(|m| {
                let last = all.get(self.conversations.members(m.id).last()?)?;
                Some((m.id, (last.timestamp, last.received.clone())))
            })
            .collect()
    }

    /// The Inbox is shown and suspicious mail is held apart from it.
    fn holding(&self) -> bool {
        self.settings.hold_suspicious && self.role() == Role::Inbox
    }

    /// Checks, in the background, the messages of the Inbox just loaded that have no verdict yet. A
    /// verdict is kept in the archive and stays until a mark changes it or a full check is asked for,
    /// so only new mail is ever checked.
    fn screen_inbox(&mut self, serial: u64, index: usize) {
        if !self.holding() {
            return;
        }
        let Some(view) = self.view(serial) else { return };
        let (Some(source), account) = (view.source.clone(), view.account.clone()) else { return };
        let mine = |id: &MessageId| self.origin.get(id) == Some(&serial);
        // The verdicts kept from before, read at once so held mail never shows in the Inbox.
        if !self.screening.keys().any(mine)
            && let Some(folder) = view.mailboxes.get(index).map(|f| f.name.clone())
            && let Ok(kept) = self.archive.verdicts(&account.key(), &folder)
        {
            self.screening.extend(kept.into_iter().filter_map(|(id, json)| Some((id, serde_json::from_str(&json).ok()?))));
        }
        self.move_marked_spam();
        let mine = |id: &MessageId| self.origin.get(id) == Some(&serial);
        let fresh: Vec<MessageSummary> = self.messages.iter().filter(|m| mine(&m.id) && !self.screening.contains_key(&m.id)).cloned().collect();
        if fresh.is_empty() {
            return;
        }
        // Mail already held is part of the campaigns the new mail is compared with.
        let held: Vec<MessageSummary> = self.messages.iter().filter(|m| mine(&m.id) && self.screening.get(&m.id).is_some_and(|s| s.place == screen::Place::Spam)).cloned().collect();
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_owned()).collect();
        let (archive, settings) = (self.archive.clone(), self.settings.clone());
        self.checking = true;
        self.spawn(async move {
            let verdicts = screen::Known::load(&archive, &account, &own, &settings).and_then(|known| {
                let verdicts: std::collections::HashMap<MessageId, screen::Screened> = fresh.iter().map(|m| m.id).zip(screen::screen_all(&source, &known, &settings, &source.auth_server(), &fresh, &held)).collect();
                archive.set_verdicts(&kept(&verdicts))?;
                Ok(verdicts)
            });
            Event::Screened(serial, index, verdicts)
        });
    }

    /// Mail from senders the user marked as spam goes to the server's Spam folder, as the mark promised,
    /// rather than waiting under Suspicious for a verdict already given.
    fn move_marked_spam(&mut self) {
        let ids: Vec<MessageId> = self.messages.iter().filter(|m| self.screening.get(&m.id).is_some_and(|s| s.place == screen::Place::Spam && self.settings.mark_of(&s.key) == Some(false))).map(|m| m.id).collect();
        for id in ids {
            self.act(id, Action::Spam);
        }
    }

    /// Messages of the Inbox being shown that are held as suspicious. Counted from the messages listed,
    /// not from the verdicts: a verdict outlives its message moved or deleted meanwhile.
    fn held(&self) -> usize {
        self.messages.iter().filter(|m| self.screening.get(&m.id).is_some_and(|s| s.place == screen::Place::Spam)).count()
    }

    /// Checks the whole Inbox being shown again, after the setting or a mark was forgotten, or when
    /// asked from its menu: the kept verdicts are dropped first.
    fn rescreen(&mut self) {
        for (serial, index) in self.shown() {
            if let Some(view) = self.view(serial)
                && let Some(folder) = view.mailboxes.get(index)
                && let Err(e) = self.archive.clear_verdicts(&view.account.key(), &folder.name)
            {
                self.error = Some(format!("Could not reset the spam checks: {e}"));
            }
        }
        self.screening.clear();
        self.show_suspicious &= self.settings.hold_suspicious;
        for (serial, index) in self.shown() {
            self.screen_inbox(serial, index);
        }
    }

    /// The user marked the senders (or organisations) `keys` as not spam or spam: remembered, and applied
    /// to the list at once.
    fn mark_senders(&mut self, keys: &[String], not_spam: bool) {
        let mut changed = std::collections::HashMap::new();
        for key in keys {
            self.settings.set_mark(key, Some(not_spam));
            for (id, s) in self.screening.iter_mut().filter(|(_, s)| s.key == *key) {
                s.mark(not_spam);
                changed.insert(*id, s.clone());
            }
        }
        self.save_settings();
        if let Err(e) = self.archive.set_verdicts(&kept(&changed)) {
            self.error = Some(format!("Could not keep the change to the spam checks: {e}"));
        }
    }

    /// Keeps these messages as training examples of spam (or not), with the text they have now: they
    /// count at once, and still after they move or the server empties its Spam folder.
    fn learn(&mut self, ids: &[MessageId], spam: bool) {
        for id in ids {
            let account = self.serial_of(*id).and_then(|s| self.view(s)).map(|v| v.account.key());
            if let (Some(account), Some(m)) = (account, self.summary_of(*id).cloned()) {
                self.keep_examples(&account, &[m], spam);
            }
        }
    }

    /// Keeps `messages` as examples for the spam checks to learn from, whatever happens to them later.
    fn keep_examples(&mut self, account: &str, messages: &[MessageSummary], spam: bool) {
        for m in messages {
            // A message without a Message-ID is told apart by sender and subject.
            let id = if m.message_id.is_empty() { format!("{}|{}", m.address, m.subject) } else { m.message_id.clone() };
            let (text, domain) = screen::example(m);
            if let Err(e) = self.archive.add_example(account, &id, spam, &text, &domain) {
                self.error = Some(format!("Could not keep this for the spam checks to learn: {e}"));
            }
        }
    }

    /// These Inbox messages are spam: their senders are marked, they are kept as examples, and they
    /// move to the server's Spam folder, where other devices and the server's own filter learn it too.
    fn move_to_spam(&mut self, ids: &[MessageId]) {
        let mut keys: Vec<String> = ids.iter().filter_map(|id| self.summary_of(*id)).map(|m| screen::sender_key(&m.address)).collect();
        keys.sort();
        keys.dedup();
        self.mark_senders(&keys, false);
        self.learn(ids, true);
        for id in ids {
            self.act(*id, Action::Spam);
        }
    }

    /// Applies a spam correction to message `id`, from the reader, its row or the keyboard.
    fn correct(&mut self, id: MessageId, choice: Correction) {
        let Some(key) = self.summary_of(id).map(|m| screen::sender_key(&m.address)) else { return };
        match choice {
            Correction::NotSpam => {
                self.mark_senders(&[key], true);
                self.learn(&[id], false);
            }
            // The sender's other mail here goes too: the mark is about the sender.
            Correction::Spam => {
                let ids: Vec<MessageId> = self.messages.iter().filter(|m| screen::sender_key(&m.address) == key).map(|m| m.id).collect();
                self.move_to_spam(&ids);
            }
            Correction::OutOfSpam => self.not_spam(id),
        }
    }

    /// The correction a message of the folder being shown offers, if any, with the check that
    /// failed: none for mail from known senders.
    fn correction(&self, id: MessageId) -> Option<(Correction, Option<&str>)> {
        match self.role_of(id) {
            Role::Junk => Some((Correction::OutOfSpam, None)),
            Role::Inbox if self.settings.hold_suspicious => self.screening.get(&id).and_then(|s| {
                let warning = s.findings.first().map(|f| f.text.as_str());
                match s.place {
                    screen::Place::Spam => Some((Correction::NotSpam, warning.or(Some(s.why.as_str())))),
                    screen::Place::Inbox if s.new_sender => Some((Correction::Spam, warning)),
                    screen::Place::Inbox => None,
                }
            }),
            _ => None,
        }
    }

    /// Trusts the sender (or organisation) of message `id`: its mail reaches the Inbox from now on, unless
    /// the server says it is forged, and this message counts as an example of wanted mail.
    fn trust(&mut self, id: MessageId) {
        let Some(key) = self.summary_of(id).map(|m| screen::sender_key(&m.address)) else { return };
        self.mark_senders(std::slice::from_ref(&key), true);
        self.learn(&[id], false);
        self.notice = Some((format!("Trusted {}", key.trim_start_matches('@')), std::time::Instant::now()));
    }

    /// A message of the Spam folder is not spam: its sender is marked so, and it goes back to the Inbox.
    fn not_spam(&mut self, id: MessageId) {
        let Some(key) = self.summary_of(id).map(|m| screen::sender_key(&m.address)) else { return };
        self.mark_senders(&[key], true);
        self.learn(&[id], false);
        self.act(id, Action::NotSpam);
    }

    fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Asks every account's archive for the query typed in the search box.
    fn run_search(&mut self) {
        self.searched.clone_from(&self.query);
        self.search_seq += 1;
        let (seq, query) = (self.search_seq, self.query.trim().to_owned());
        if query.is_empty() {
            self.hits.clear();
            return;
        }
        // The folder shown (every account's, in All inboxes), unless the query names one: "in:all" is everywhere.
        let named = query.split_whitespace().any(|w| w.len() > 3 && w[..3].eq_ignore_ascii_case("in:"));
        let shown: std::collections::HashMap<u64, String> =
            if named || self.show_scheduled { Default::default() } else { self.shown().into_iter().filter_map(|(s, i)| Some((s, self.view(s)?.mailboxes.get(i)?.name.clone()))).collect() };
        let sources: Vec<(u64, Arc<ImapSource>)> = self.accounts.iter().filter_map(|v| Some((v.serial, v.source.clone()?))).collect();
        for (serial, source) in sources {
            let query = match shown.get(&serial) {
                Some(folder) => format!("{query} in:\"{folder}\""),
                None if shown.is_empty() => query.clone(),
                None => continue,
            };
            self.spawn(async move { Event::Found(seq, serial, source.search(&query, 200).await) });
        }
    }

    /// A search result was clicked: show its folder with the message open.
    /// Shows a search result where it is, without leaving the search: its folder need not be the one shown.
    fn open_hit(&mut self, serial: u64, hit: &Hit, pin: bool) {
        let Some(role) = self.view(serial).and_then(|v| v.mailboxes.iter().find(|m| m.name == hit.folder)).map(|m| m.role) else { return };
        self.open(tabs::Tab { serial, message: hit.message.clone(), role, pinned: pin, source: false });
    }

    /// Asks the server again for the selected folder and its account's folder list.
    fn refresh(&mut self) {
        for (serial, index) in self.shown() {
            let Some(view) = self.view(serial) else { continue };
            let (Some(source), Some(mailbox)) = (view.source.clone(), view.mailboxes.get(index)) else { continue };
            let name = mailbox.name.clone();
            let folders = source.clone();
            self.spawn(async move { Event::Mailboxes(serial, folders.mailboxes().await, true) });
            self.start_sync(serial, index, source, name);
            self.start_download(serial);
        }
    }

    /// Accounts that can send, as (serial, address).
    fn senders(&self) -> Vec<(u64, String)> {
        self.accounts.iter().filter(|v| v.sender.is_some()).map(|v| (v.serial, v.account.address().to_owned())).collect()
    }

    /// Opens the compose window from the selected account, or the first one that can send.
    fn new_message(&mut self) {
        self.new_message_to("");
    }

    /// Opens the compose window addressed to `to` (a list item, or nothing).
    fn new_message_to(&mut self, to: &str) {
        if self.compose.is_some() {
            return;
        }
        let senders = self.senders();
        // The account of the folder shown, or in All inboxes the open message's.
        let shown = self.selected.map(|(s, _)| s).or_else(|| self.message.and_then(|id| self.serial_of(id)));
        let from = shown.filter(|s| senders.iter().any(|(x, _)| x == s)).or_else(|| senders.first().map(|(s, _)| *s));
        let signature = from.and_then(|f| self.view(f)).map(|v| v.account.signature.clone()).unwrap_or_default();
        self.start_compose(from.map(|f| compose::Compose::new(f).with_signature(&signature).with_to(to)));
    }

    /// Shows a compose window, remembering which account's drafts it belongs to.
    fn start_compose(&mut self, compose: Option<compose::Compose>) {
        if let Some(c) = &compose {
            // Fresh for each message, ranked for its account; the user's own addresses are not suggested.
            let (archive, key) = (self.archive.clone(), self.view(c.from).map(|v| v.account.key()).unwrap_or_default());
            let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_lowercase()).collect();
            self.spawn(async move { Event::Contacts(archive.contacts(&key).map(|all| all.into_iter().filter(|c| !own.contains(&c.address)).collect())) });
        }
        self.compose = compose.map(|mut c| {
            c.key = self.view(c.from).map(|v| v.account.key()).unwrap_or_default();
            c
        });
    }

    /// Files dropped onto the main window start a new message with them attached.
    fn drop_files_on_the_app(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let free = self.compose.is_none() && self.form.is_none() && !self.show_settings && !self.locked() && !self.senders().is_empty();
        if !free {
            return;
        }
        if compose::hovering_files(ctx) {
            compose::drop_hint(ctx, t, ctx.content_rect(), "Drop files to write a new message");
        }
        let files = compose::dropped(ctx);
        if !files.is_empty() {
            self.new_message();
            self.attach_files(files);
        }
    }

    /// Hands the message to the server in the background; the answer comes back as `Event::Sent`.
    fn dispatch_send(&mut self, compose: &mut compose::Compose) {
        if self.send_message(compose) {
            compose.sending = true;
            compose.error = None;
        }
    }

    /// Sends `compose`'s message from its account in the background; false when the account cannot send.
    fn send_message(&mut self, compose: &compose::Compose) -> bool {
        let Some(view) = self.view(compose.from) else { return false };
        let (account, sender, source) = (view.account.clone(), view.sender.clone(), view.source.clone());
        let Some(sender) = sender else { return false };
        let mut message = compose.outgoing(&account.sender(), self.settings.rich_outgoing);
        // With a key, it goes out with each message, so people's programs learn it (Autocrypt).
        message.autocrypt = self.own_key(account.address()).and_then(|secret| beavermail_crypto::public_key(&secret));
        match (compose.encrypt_auto, compose.encrypt, self.seal_keys(compose)) {
            // Asked to encrypt but it cannot be: never sent in the clear instead.
            (false, true, Err(why)) => {
                self.error = Some(format!("Not sent: it cannot be encrypted. {why}"));
                return false;
            }
            (true, _, Ok(seal)) | (false, true, Ok(seal)) => message.seal = Some(seal),
            _ => {}
        }
        let (files, vault, id) = (compose.file_list(), self.vault.clone(), compose.id.clone());
        self.spawn(async move {
            // The copies are read now, off the window's thread, as they can be large.
            match tokio::task::spawn_blocking(move || attachments::read_files(&vault, &files)).await {
                Ok(Ok(read)) => message.attachments = read,
                Ok(Err(e)) => return Event::Sent(id, Err(MailError::Backend(e)), None),
                Err(e) => return Event::Sent(id, Err(MailError::Backend(e.to_string())), None),
            }
            let result = sender.send(message).await;
            let copy_error = match (&result, source) {
                (Ok(raw), Some(source)) if account.save_sent => source.save_sent(raw.clone()).await.err().map(|e| e.to_string()),
                _ => None,
            };
            Event::Sent(id, result.map(|_| ()), copy_error)
        });
        true
    }

    /// Puts the compose window's message in the Outbox: kept as a draft marked to send, the window closes.
    /// `send_at`: not before then ("send later"); `None` as soon as the server can be reached.
    fn queue(&mut self, compose: compose::Compose, send_at: Option<i64>) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let draft = drafts::DraftData { queued: true, send_at, ..compose.to_draft(now) };
        let told = match send_at {
            Some(at) => format!("Scheduled for {}. BeaverMail sends it then, if it is open", compose::when_text(at)),
            None => "Saved to the Outbox: it is sent when the server can be reached".to_owned(),
        };
        match drafts::save(&self.vault, &compose.key, &draft) {
            Ok(()) => self.notice = Some((told, std::time::Instant::now())),
            Err(e) => {
                self.error = Some(format!("Could not keep the message to send later: {e}"));
                self.compose = Some(compose); // nothing is lost: the window stays
                return;
            }
        }
        self.count_outbox();
        self.refresh_drafts();
    }

    /// Sends what waits in account `serial`'s Outbox; its server just answered.
    fn send_outbox(&mut self, serial: u64) {
        let Some(key) = self.view(serial).map(|v| v.account.key()) else { return };
        // Changes made offline go first, in the order they were made.
        if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
            let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.offline_changes).into_iter().partition(|c| c.0 == serial);
            self.offline_changes = later;
            if !now.is_empty() {
                self.save_offline(); // one that fails offline again comes back through Event::Done
            }
            for (serial, index, id, action) in now {
                self.change(source.clone(), serial, index, id, action);
            }
        }
        let open = self.compose.as_ref().map(|c| c.id.clone());
        let now = chrono::Local::now().timestamp();
        for draft in drafts::load_all(&self.vault, &key).into_iter().filter(|d| d.queued && d.send_at.is_none_or(|at| at <= now)) {
            // One being edited, or already on its way, is left alone.
            if open.as_ref() == Some(&draft.id) || self.outbox_sending.contains_key(&draft.id) {
                continue;
            }
            let mut compose = compose::Compose::from_draft(serial, draft.clone());
            compose.key = key.clone();
            if self.send_message(&compose) {
                self.outbox_sending.insert(draft.id.clone(), (serial, draft));
            }
        }
    }

    /// Counts the messages in every account's Outbox, for the status bar.
    fn count_outbox(&mut self) {
        self.outbox = 0;
        self.scheduled.clear();
        let now = chrono::Local::now().timestamp();
        for view in &self.accounts {
            for draft in drafts::load_all(&self.vault, &view.account.key()).into_iter().filter(|d| d.queued) {
                match draft.send_at {
                    Some(at) if at > now => self.scheduled.push((view.serial, draft)),
                    // Its time has come: it waits like any Outbox message for the server to answer, so a
                    // send that fails offline is not tried again every frame.
                    _ => self.outbox += 1,
                }
            }
        }
    }

    /// Sends the scheduled messages whose time has come, and wakes the window for the next one.
    fn send_scheduled(&mut self, ctx: &egui::Context) {
        let now = chrono::Local::now().timestamp();
        let at = |d: &drafts::DraftData| d.send_at.unwrap_or(i64::MAX);
        let due: Vec<u64> = self.scheduled.iter().filter(|(_, d)| at(d) <= now).map(|(serial, _)| *serial).collect();
        if !due.is_empty() {
            // Once each: the answer counts the Outbox again.
            self.scheduled.retain(|(_, d)| at(d) > now);
            for serial in due {
                self.send_outbox(serial);
            }
        }
        if let Some(next) = self.scheduled.iter().map(|(_, d)| at(d)).min() {
            ctx.request_repaint_after(std::time::Duration::from_secs((next - now).clamp(1, 60) as u64));
        }
    }

    fn compose_window(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(mut compose) = self.compose.take() else { return };
        // Files dragged onto the window while composing.
        let dropped = compose::dropped(ctx);
        if !dropped.is_empty() {
            self.compose = Some(compose);
            self.attach_files(dropped);
            let Some(back) = self.compose.take() else { return };
            compose = back;
        }
        let senders = self.senders();
        // The focus is dropped again the frame after the question closes, so it is asked for a few times.
        if self.refocus > 0 && let Some(field) = self.compose_focus {
            self.refocus -= 1;
            ctx.memory_mut(|m| m.request_focus(field));
            ctx.request_repaint();
        }
        if !self.confirm_discard && let Some(focused) = ctx.memory(|m| m.focused()) {
            self.compose_focus = Some(focused);
        }
        // Esc is Discard (asked first when there is text), unless it is meant for a menu or the question itself.
        let esc = !self.confirm_discard && !compose.sending && !compose.picking && !compose.suggesting && !egui::Popup::is_any_open(ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        compose.contacts = self.contacts.clone();
        self.check_encryption(&mut compose);
        let outcome = if esc { compose::Outcome::Discard } else { compose::window(ctx, t, &mut compose, &senders, self.settings.rich_outgoing) };
        if compose.from != compose.sig_from {
            let signature = self.view(compose.from).map(|v| v.account.signature.clone()).unwrap_or_default();
            compose.change_signature(compose.from, &signature);
        }
        match outcome {
            compose::Outcome::Keep => {}
            compose::Outcome::Close => {
                self.save_draft(&mut compose);
                if !compose.is_empty() && compose.snapshot() != compose.server_saved {
                    self.push_draft(&mut compose);
                }
                return;
            }
            compose::Outcome::Discard if compose.is_empty() => {
                self.delete_draft(&compose);
                return;
            }
            compose::Outcome::Discard => self.confirm_discard = true,
            compose::Outcome::Attach => {
                self.spawn(async {
                    let files = rfd::AsyncFileDialog::new().set_title("Attach files").pick_files().await;
                    Event::Picked(files.unwrap_or_default().into_iter().map(|f| f.path().to_path_buf()).collect())
                });
            }
            compose::Outcome::Send if self.settings.undo_send_secs > 0 => {
                compose.sending = true;
                compose.error = None;
                compose.send_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(self.settings.undo_send_secs.into()));
            }
            compose::Outcome::Send => self.dispatch_send(&mut compose),
            compose::Outcome::Undo => {
                compose.sending = false;
                compose.send_at = None;
            }
            compose::Outcome::Schedule(at) => {
                self.queue(compose, Some(at));
                return;
            }
        }
        if compose.send_at.is_some_and(|at| at <= std::time::Instant::now()) {
            compose.send_at = None;
            self.dispatch_send(&mut compose);
        }
        // Keep a draft of what is being written, once the text has been still for a moment.
        if !compose.sending {
            if compose.autosave_due(std::time::Duration::from_secs(2)) {
                self.save_draft(&mut compose);
            } else if compose.unsaved() {
                ctx.request_repaint_after(std::time::Duration::from_millis(600));
            }
            // A copy on the server after a longer pause: it is uploaded whole, attachments included.
            if compose.server_due(std::time::Duration::from_secs(15)) && !compose.unsaved() {
                self.push_draft(&mut compose);
            } else if !compose.is_empty() && compose.snapshot() != compose.server_saved {
                ctx.request_repaint_after(std::time::Duration::from_secs(2));
            }
        }
        if self.confirm_discard {
            let (mut discard, mut keep, mut save) = (false, false, false);
            let quitting = self.quitting;
            let modal = egui::Modal::new(egui::Id::new("confirm_discard")).show(ctx, |ui| {
                ui.set_width(FORM_WIDTH * 0.8);
                ui.heading(if quitting { "Quit with this message open?" } else { "Discard this message?" });
                ui.label(RichText::new("What you wrote will be lost, unless you keep it as a draft.").color(t.text_dim));
                ui.add_space(12.0);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                    let button = egui::Button::new(accelerated(if quitting { "Discard and quit" } else { "Discard" }, 0, t.on_accent)).fill(t.danger);
                    discard = ui.add(button).clicked();
                    save = ui.button(accelerated(if quitting { "Save draft and quit" } else { "Save draft" }, 0, egui::Color32::PLACEHOLDER)).clicked();
                    keep = ui.button(accelerated("Keep editing", 0, egui::Color32::PLACEHOLDER)).clicked();
                });
            });
            let letter = |k| ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
            let (discard, save) = (discard || letter(egui::Key::D), save || letter(egui::Key::S));
            let keep = keep || letter(egui::Key::K);
            // Asked because the app is closing: it closes now, the window being gone.
            let close = |ctx: &egui::Context| {
                if quitting {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            };
            if discard {
                self.confirm_discard = false;
                self.delete_draft(&compose);
                close(ctx);
                return;
            }
            if save {
                self.confirm_discard = false;
                self.save_draft(&mut compose);
                // Not kept (the error says why): the app stays open with the message, rather than lose it.
                if quitting && compose.unsaved() {
                    self.quitting = false;
                    self.compose = Some(compose);
                    return;
                }
                if !compose.is_empty() && compose.snapshot() != compose.server_saved {
                    self.push_draft(&mut compose);
                }
                close(ctx);
                return;
            }
            if keep || modal.should_close() {
                self.confirm_discard = false;
                self.quitting = false;
                self.refocus = 3;
            }
        }
        self.compose = Some(compose);
    }

    /// Closing waits for the changes in flight; Quit now keeps them for the next run instead.
    fn closing_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        if !self.closing {
            return;
        }
        let n = self.in_flight.len();
        let mut quit = n == 0;
        if n > 0 {
            egui::Modal::new(egui::Id::new("closing")).show(ctx, |ui| {
                ui.set_width(FORM_WIDTH * 0.8);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0));
                    ui.heading("Finishing changes on the server");
                });
                let text = format!("BeaverMail closes once the server confirms {n} change{}.", if n == 1 { "" } else { "s" });
                ui.label(RichText::new(text).color(t.text_dim));
                ui.add_space(12.0);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                    quit = ui.button(accelerated("Quit now", 0, egui::Color32::PLACEHOLDER)).on_hover_text("The changes are made at the next start").clicked();
                });
            });
            quit |= ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Q));
        }
        if quit {
            if n > 0 {
                self.offline_changes.append(&mut self.in_flight);
                self.save_offline();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The stored mail is protected by a passphrase that has not been entered, and the prompt is showing.
    fn locked(&self) -> bool {
        self.vault.is_locked() && !self.unlock_skipped
    }

    /// What the user did to an attachment chip.
    /// The text of open message `id` of the reader: the one the tab is for, or another of its conversation.
    fn body_of(&self, id: MessageId) -> Option<&OpenBody> {
        if self.message == Some(id) {
            return self.body.as_ref();
        }
        self.unfolded.iter().find(|u| u.id == id).and_then(|u| u.body.as_ref()?.as_ref().ok())
    }

    fn use_attachment(&mut self, id: MessageId, index: u32, what: views::ChipAction) {
        let name = self.body_of(id).and_then(|b| b.data.attachments.iter().find(|a| a.index == index)).map(|a| a.name.clone());
        match what {
            views::ChipAction::Toggle => {
                if !self.attach_selected.remove(&(id.0, index)) {
                    self.attach_selected.insert((id.0, index));
                }
            }
            views::ChipAction::SaveAs => self.save_attachments(id, vec![index]),
            views::ChipAction::Open => {
                if let Some(name) = name.filter(|n| attachments::is_risky(n)) {
                    self.confirm_open = Some((id, index, name));
                } else {
                    self.fetch_attachment(id, index);
                }
            }
        }
    }

    /// The ticked attachments of message `id`, or all of them when none is ticked.
    fn chosen_attachments(&self, id: MessageId) -> Vec<u32> {
        let ticked: Vec<u32> = self.attach_selected.iter().filter(|(m, _)| *m == id.0).map(|(_, i)| *i).collect();
        if ticked.is_empty() { self.body_of(id).iter().flat_map(|b| b.data.attachments.iter().map(|a| a.index)).collect() } else { ticked }
    }

    fn fetch_attachment(&mut self, id: MessageId, index: u32) {
        let Some(source) = self.serial_of(id).and_then(|s| self.view(s)?.source.clone()) else { return };
        self.spawn(async move { Event::Attachment(source.attachment(id, index).await) });
    }

    /// Asks where to save, then saves: one attachment into a file of the user's choosing, several into
    /// a folder (names that exist get a number, nothing is overwritten).
    fn save_attachments(&mut self, id: MessageId, indexes: Vec<u32>) {
        let Some(source) = self.serial_of(id).and_then(|s| self.view(s)?.source.clone()) else { return };
        let names: Vec<(u32, String)> = indexes.iter().filter_map(|i| self.body_of(id)?.data.attachments.iter().find(|a| a.index == *i)).map(|a| (a.index, attachments::safe_name(&a.name))).collect();
        let Some(first) = names.first().map(|(_, n)| n.clone()) else { return };
        self.spawn(async move {
            let one = names.len() == 1;
            let dialog = rfd::AsyncFileDialog::new();
            let target = if one { dialog.set_title("Save attachment").set_file_name(&first).save_file().await } else { dialog.set_title("Save attachments to…").pick_folder().await };
            let Some(target) = target else { return Event::Saved(Ok(None)) };
            let target = target.path().to_path_buf();
            let mut saved = Vec::new();
            for (index, name) in &names {
                let (_, bytes) = match source.attachment(id, *index).await {
                    Ok(file) => file,
                    Err(e) => return Event::Saved(Err(format!("Could not get {name}: {e}"))),
                };
                let path = if one { target.clone() } else { attachments::unique(&target, name) };
                if let Err(e) = std::fs::write(&path, &bytes) {
                    return Event::Saved(Err(format!("{}: {e}", path.display())));
                }
                saved.push(path);
            }
            Event::Saved(Ok(Some(if one { format!("Saved to {}", saved[0].display()) } else { format!("Saved {} files to {}", saved.len(), target.display()) })))
        });
    }

    fn open_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some((id, index, name)) = self.confirm_open.clone() else { return };
        let (mut open, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_open")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.85);
            ui.heading("Open this file?");
            ui.label(RichText::new(format!("{} can run programs or change your computer when opened. Only open it if you were expecting it from someone you trust.", attachments::safe_name(&name))).color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                open = ui.add(egui::Button::new(RichText::new("Open anyway").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if open {
            self.fetch_attachment(id, index);
        }
        if open || cancel || modal.should_close() {
            self.confirm_open = None;
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = store::save_settings(&self.settings) {
            self.error = Some(e);
        }
    }

    /// Starts a change to how stored mail is protected, on a worker thread.
    fn run_vault(&mut self, request: security::Request, ctx: &egui::Context) {
        let (vault, archive, tx, ctx) = (self.vault.clone(), self.archive.clone(), self.tx.clone(), ctx.clone());
        match request {
            security::Request::Skip => self.unlock_skipped = true,
            security::Request::Apply(mode, passphrase) => {
                let job = Arc::new(security::Job::default());
                self.vault_job = Some(job.clone());
                self.runtime.spawn_blocking(move || {
                    let progress = |done: usize, total: usize| {
                        job.done.store(done, std::sync::atomic::Ordering::Relaxed);
                        job.total.store(total, std::sync::atomic::Ordering::Relaxed);
                        ctx.request_repaint();
                    };
                    let result = vault.set_mode(mode, passphrase.as_deref(), &progress).and_then(|()| archive.rekey(vault.derived_key("index").as_deref()));
                    let _ = tx.send(Event::Vault(VaultOutcome::Applied(mode, result)));
                    ctx.request_repaint();
                });
            }
            security::Request::Unlock(passphrase) => {
                self.unlocking = true;
                self.runtime.spawn_blocking(move || {
                    let _ = tx.send(Event::Vault(VaultOutcome::Unlocked(vault.unlock(&passphrase).and_then(|()| archive.unlock(vault.derived_key("index").as_deref())))));
                    ctx.request_repaint();
                });
            }
            security::Request::Recover(code) => {
                self.unlocking = true;
                self.runtime.spawn_blocking(move || {
                    let _ = tx.send(Event::Vault(VaultOutcome::Recovered(vault.recover(&code).and_then(|()| archive.unlock(vault.derived_key("index").as_deref())))));
                    ctx.request_repaint();
                });
            }
            security::Request::CreateRecovery => match self.vault.add_recovery() {
                Ok(code) => self.security.show_code(code),
                Err(e) => self.error = Some(format!("Could not make a recovery key: {e}")),
            },
            security::Request::RemoveRecovery => match self.vault.remove_recovery() {
                Ok(()) => self.notice = Some(("Recovery key removed".into(), std::time::Instant::now())),
                Err(e) => self.error = Some(format!("Could not remove the recovery key: {e}")),
            },
            security::Request::Reset => {
                self.unlocking = true;
                self.runtime.spawn_blocking(move || {
                    let _ = tx.send(Event::Vault(VaultOutcome::Reset(vault.reset().and_then(|()| archive.reset(vault.derived_key("index").as_deref())))));
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Saves a recovery key just made to a text file the user picks.
    fn save_recovery(&mut self, code: String) {
        self.spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new().set_title("Save recovery key").set_file_name("BeaverMail recovery key.txt").save_file().await else { return Event::Saved(Ok(None)) };
            let text = format!("BeaverMail recovery key\n\n{code}\n\nIt opens the mail stored by BeaverMail on your computer if its key or your passphrase is lost.\n");
            Event::Saved(std::fs::write(file.path(), text).map(|()| Some(format!("Saved to {}", file.path().display()))).map_err(|e| format!("{}: {e}", file.path().display())))
        });
    }

    fn apply_vault(&mut self, outcome: VaultOutcome) {
        self.unlocking = false;
        match outcome {
            VaultOutcome::Applied(mode, result) => {
                self.vault_job = None;
                self.security.clear_passphrases();
                self.security.mode = None;
                match result {
                    Ok(()) => {
                        let text = match mode {
                            Mode::Off => "Stored mail is no longer encrypted",
                            Mode::System => "Stored mail is encrypted with a key kept by this computer",
                            Mode::Passphrase => "Stored mail is encrypted with your passphrase",
                        };
                        self.notice = Some((text.into(), std::time::Instant::now()));
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            VaultOutcome::Unlocked(Ok(())) => {
                self.security.clear_unlock();
                self.load_offline();
            }
            VaultOutcome::Unlocked(Err(MailError::Auth(_))) => self.security.unlock_error = Some("That passphrase does not open the stored mail.".into()),
            VaultOutcome::Unlocked(Err(e)) => self.security.unlock_error = Some(e.to_string()),
            VaultOutcome::Recovered(Ok(())) => {
                self.load_offline();
                self.security.clear_unlock();
                self.security.mode = None;
                self.notice = Some(("Stored mail is open again, unlocked by this computer from now on".into(), std::time::Instant::now()));
            }
            VaultOutcome::Recovered(Err(MailError::Auth(_))) => self.security.unlock_error = Some("That recovery key does not open the stored mail. Check it and try again.".into()),
            VaultOutcome::Recovered(Err(e)) => self.security.unlock_error = Some(e.to_string()),
            VaultOutcome::Reset(Ok(())) => {
                self.security.clear_unlock();
                self.security.mode = None;
                self.notice = Some(("Stored mail was deleted and will be downloaded again".into(), std::time::Instant::now()));
            }
            VaultOutcome::Reset(Err(e)) => self.error = Some(e.to_string()),
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        if !self.show_settings {
            return;
        }
        // The page of keys reads them here: the vault is not read while the window draws.
        let key_ids: Vec<encryption::Keys> = if self.settings_page == SettingsPage::Encryption {
            let addresses: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_lowercase()).collect();
            addresses
                .into_iter()
                .map(|a| encryption::Keys { openpgp: self.own_key(&a).and_then(|k| beavermail_crypto::short_id(&k)), smime: self.own_certificate(&a).and_then(|c| beavermail_crypto::certificate_info(&c)), address: a })
                .collect()
        } else {
            Vec::new()
        };
        let mut key_action = None;
        // An account page edits a copy of the account; the page of a removed account falls back to the first page.
        if let SettingsPage::Account(serial) = self.settings_page {
            match self.view(serial) {
                Some(view) if self.account_edit.as_ref().is_none_or(|f| f.editing != Some(serial)) => {
                    self.account_edit = Some(NewAccount::editing(serial, &view.account));
                }
                Some(_) => {}
                None => self.leave_settings_page(Some(SettingsPage::Reading)),
            }
        }
        let account = match self.settings_page {
            SettingsPage::Account(serial) => self.view(serial).map(|v| v.account.clone()),
            _ => None,
        };
        let dirty = account.as_ref().zip(self.account_edit.as_ref()).is_some_and(|(a, form)| form.changed(a));
        let mut changed = false;
        let mut rescreen = false;
        let mut vault_request = None;
        let mut unlock_again = false;
        // Where the user asked to go: another page, or `None` to close Settings.
        let mut goto: Option<Option<SettingsPage>> = None;
        let (mut save, mut discard, mut add) = (false, false, false);
        let mut page_action = None;
        let (mode, locked, job) = (self.vault.mode(), self.vault.is_locked(), self.vault_job.clone());
        // The same height on every page, so the window does not jump when the page changes.
        let height = (ctx.content_rect().height() - 120.0).clamp(380.0, 640.0);
        let frame = egui::Frame::window(&ctx.global_style()).inner_margin(0);
        let modal = egui::Modal::new(egui::Id::new("settings")).frame(frame).show(ctx, |ui| {
            ui.set_width(SETTINGS_WIDTH);
            ui.set_height(height);
            // Painted once the sidebar's width is known, behind it and down to the window's bottom edge.
            let nav_fill = ui.painter().add(egui::Shape::Noop);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let nav = egui::Frame::new().inner_margin(12);
                let nav = nav.show(ui, |ui| ui.vertical(|ui| {
                    ui.set_width(SETTINGS_NAV - 24.0);
                    ui.set_height(height - 24.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let label = |ui: &mut egui::Ui, text: RichText| {
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(text);
                        });
                    };
                    ui.add_space(6.0);
                    label(ui, RichText::new("Settings").heading());
                    ui.add_space(14.0);
                    for page in SettingsPage::ALL {
                        if views::settings_row(ui, t, page.title(), page.icon(), self.settings_page == page).clicked() && self.settings_page != page {
                            goto = Some(Some(page));
                        }
                    }
                    ui.add_space(18.0);
                    label(ui, RichText::new("Accounts").size(13.0).family(theme::bold()).color(t.text_dim));
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().id_salt("settings_accounts").show(ui, |ui| {
                        for view in &self.accounts {
                            let page = SettingsPage::Account(view.serial);
                            let row = views::settings_row(ui, t, view.account.address(), None, self.settings_page == page);
                            if row.clicked() && self.settings_page != page {
                                goto = Some(Some(page));
                            }
                        }
                        add = views::settings_row(ui, t, "Add account", Some(Icon::Plus), false).clicked();
                    });
                }));
                let side = egui::Rect::from_min_size(nav.response.rect.min, vec2(nav.response.rect.width(), height));
                ui.painter().set(nav_fill, egui::epaint::RectShape::filled(side, egui::CornerRadius { nw: 14, sw: 14, ne: 0, se: 0 }, t.bg));
                ui.painter().vline(side.right(), side.y_range(), egui::Stroke::new(1.0, t.border));
                egui::Frame::new().inner_margin(egui::Margin { left: 28, right: 24, top: 26, bottom: 20 }).show(ui, |ui| ui.vertical(|ui| {
                    ui.set_width(SETTINGS_WIDTH - SETTINGS_NAV - 52.0);
                    ui.set_height(height - 46.0);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    match &account {
                        Some(account) => {
                            ui.horizontal(|ui| {
                                let (rect, _) = ui.allocate_exact_size(vec2(views::AVATAR, views::AVATAR), egui::Sense::hover());
                                views::avatar(ui, t, account.address(), rect.center(), views::AVATAR);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(RichText::new(account.address()).heading());
                                    let sent_as = account.sender_name().map_or("Sent with the address alone".to_owned(), |name| format!("Sent as {name}"));
                                    ui.label(RichText::new(sent_as).color(t.text_dim));
                                });
                            });
                        }
                        None => {
                            ui.label(RichText::new(self.settings_page.title()).heading());
                            ui.label(RichText::new(self.settings_page.about()).color(t.text_dim));
                        }
                    }
                    ui.add_space(14.0);
                    // What is left once the footer, its gap and the spacing around both are taken off.
                    let body = (ui.available_height() - theme::CONTROL_H - 8.0 - 2.0 * ui.spacing().item_spacing.y).max(0.0);
                    egui::ScrollArea::vertical().id_salt(("settings_page", self.settings_page)).max_height(body).min_scrolled_height(body).auto_shrink([false, false]).show(ui, |ui| {
                        // Room between the page and its scroll bar.
                        ui.set_max_width(ui.available_width() - 16.0);
                        let hint = |ui: &mut egui::Ui, text: &str| ui.label(RichText::new(text).size(13.0).color(t.text_dim));
                        match self.settings_page {
                            SettingsPage::Account(_) => {
                                if let Some(form) = &mut self.account_edit {
                                    let waiting = self.signing_in.as_ref().is_some_and(|s| s.editing.is_some() && s.editing == form.editing);
                                    page_action = account_page(ui, t, form, waiting);
                                }
                            }
                            SettingsPage::Reading => {
            let modes = [
                (DisplayMode::Plain, "Plain text", "The safest view. Links stay clickable; images and styling are dropped."),
                (DisplayMode::Simplified, "Simplified HTML", "Keeps headings, lists, quotes and links. Never loads images, fonts or trackers."),
                (DisplayMode::Original, "Original HTML", "Layout and colours as the sender designed them. Nothing is loaded from a server, so no tracking."),
            ];
            for (mode, title, hint) in modes {
                if ui.radio(self.settings.display == mode, title).clicked() && self.settings.display != mode {
                    self.settings.display = mode;
                    changed = true;
                }
                ui.label(RichText::new(hint).size(13.0).color(t.text_dim));
                ui.add_space(4.0);
            }
                                ui.add_space(12.0);
                                let about = "One row per conversation, with your replies from Sent. Archive, Delete and Mark as read act on all of it.";
                                changed |= views::switch_row(ui, t, &mut self.settings.threads, "Group messages into conversations", about).changed();
                                let about = "Off: the list takes the whole width. A click selects a message; Enter or a double click opens it in a tab.";
                                changed |= views::switch_row(ui, t, &mut self.settings.reading_pane, "Show the message beside the list", about).changed();
                                section_title(ui, "Links");
                                hint(ui, "A link opens straight where it leads, without the redirects of Google, Outlook, Facebook and others, and without known tracking parameters (utm_…, fbclid, gclid and more). Right-click a link to copy it clean.");
                                ui.add_space(4.0);
                                ui.label("Also remove these parameters");
                                changed |= ui.add(field(&mut self.settings.link_params).hint_text("ref, src, trk_*")).changed();
                                hint(ui, "Separated by commas or spaces; a name ending in * removes every parameter that starts so.");
                                ui.add_space(12.0);
            if !self.settings.remote_senders.is_empty() {
                ui.label(RichText::new("Senders that may load remote content").family(theme::bold()));
                let mut forget = None;
                for address in &self.settings.remote_senders {
                    ui.horizontal(|ui| {
                        ui.label(address);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Remove").clicked() {
                                forget = Some(address.clone());
                            }
                        });
                    });
                }
                if let Some(address) = forget {
                    self.settings.set_remote(&address, false);
                    changed = true;
                }
            }
            if !self.settings.sender_display.is_empty() {
                ui.label(RichText::new("Senders with their own setting").family(theme::bold()));
                let mut forget = None;
                for (address, mode) in &self.settings.sender_display {
                    ui.horizontal(|ui| {
                        let label = match mode { DisplayMode::Original => "Original HTML", DisplayMode::Simplified => "Simplified HTML", DisplayMode::Plain => "Plain text" };
                        ui.label(format!("{address}  ·  {label}"));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Remove").clicked() {
                                forget = Some(address.clone());
                            }
                        });
                    });
                }
                if let Some(address) = forget {
                    self.settings.sender_display.remove(&address);
                    changed = true;
                }
            }
                            }
                            SettingsPage::Appearance => {
                                ui.label("Density");
                                ui.horizontal(|ui| {
                                    for (density, name) in [(Density::Compact, "Compact"), (Density::Normal, "Normal"), (Density::Comfortable, "Comfortable")] {
                                        if ui.radio(self.settings.density == density, name).clicked() && self.settings.density != density {
                                            self.settings.density = density;
                                            changed = true;
                                        }
                                    }
                                });
                                hint(ui, "How close together the folders, address books and calendars sit in the sidebar.");
                            }
                            SettingsPage::Writing => {
                                let about = "Bold, lists, links and quotes. The plain-text part is always sent, readable as written. Off: plain text only.";
                                changed |= views::switch_row(ui, t, &mut self.settings.rich_outgoing, "Write in Markdown and send an HTML version too", about).changed();
                                ui.add_space(12.0);
                                ui.label("Undo send");
                                let secs = &mut self.settings.undo_send_secs;
                                egui::ComboBox::from_id_salt("undo_send").selected_text(if *secs == 0 { "Off".to_owned() } else { format!("{secs} seconds") }).show_ui(ui, |ui| {
                                    for n in [0, 5, 10, 20, 30] {
                                        changed |= ui.selectable_value(secs, n, if n == 0 { "Off".to_owned() } else { format!("{n} seconds") }).changed();
                                    }
                                });
                                hint(ui, "After Send the message waits here, so you can take it back. Off sends it at once.");
                            }
                            SettingsPage::Spam => {
                                let about = "Mail from a new sender that fails a check waits under Suspicious, beside the Inbox.";
                                if views::switch_row(ui, t, &mut self.settings.hold_suspicious, "Hold suspicious mail from new senders apart from the Inbox", about).changed() {
                                    changed = true;
                                    rescreen = true;
                                }
                                section_title(ui, "Trusted senders");
                                hint(ui, "Their mail always reaches the Inbox, unless your server says it is forged.");
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    let field = ui.add(sized_field(&mut self.trust_input, FIELD_WIDTH).hint_text("name@example.com or @example.com"));
                                    let key = trusted_key(&self.trust_input);
                                    let add = ui.add_enabled(key.is_some(), egui::Button::new("Add")).on_disabled_hover_text("An address, or @ and a domain");
                                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    if let Some(key) = key.filter(|_| add.clicked() || enter) {
                                        self.settings.set_mark(&key, Some(true));
                                        self.trust_input.clear();
                                        (changed, rescreen) = (true, true);
                                    }
                                });
                                // Spam marks only grow, so they are found by a search, never listed whole.
                                let marked_spam = self.settings.marked.values().filter(|v| !**v).count();
                                let query = self.spam_query.trim().to_lowercase();
                                let found: Vec<String> = self.settings.marked.iter().filter(|(k, v)| !**v && !query.is_empty() && k.contains(&query)).map(|(k, _)| k.clone()).collect();
                                const SHOWN: usize = 10;
                                for (title, trusted, empty) in [
                                    ("", true, "None yet. Choose Trust on a message from a new sender, or add one here."),
                                    ("Marked as spam", false, "None. Spam or Block sender on a message puts its sender here."),
                                ] {
                                    if !title.is_empty() {
                                        section_title(ui, title);
                                    }
                                    let keys: Vec<String> = if trusted { self.settings.marked.iter().filter(|(_, v)| **v).map(|(k, _)| k.clone()).collect() } else { found.iter().take(SHOWN).cloned().collect() };
                                    if trusted && keys.is_empty() || !trusted && marked_spam == 0 {
                                        hint(ui, empty);
                                    } else if !trusted {
                                        hint(ui, &format!("{} marked, senders and domains. Search for one to take its mark off.", views::thousands(marked_spam as u32)));
                                        ui.add_space(4.0);
                                        ui.add(sized_field(&mut self.spam_query, FIELD_WIDTH).hint_text("Search an address or domain"));
                                        if !query.is_empty() && found.is_empty() {
                                            hint(ui, "No marked sender matches.");
                                        } else if found.len() > SHOWN {
                                            hint(ui, &format!("{} more match: type more of the address.", found.len() - SHOWN));
                                        }
                                    }
                                    for key in keys {
                                        ui.horizontal(|ui| {
                                            let shown = match key.strip_prefix('@') {
                                                Some(domain) => format!("Everyone at {domain}"),
                                                None => key.clone(),
                                            };
                                            ui.label(shown);
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                if ui.button("Remove").clicked() {
                                                    self.settings.set_mark(&key, None);
                                                    (changed, rescreen) = (true, true);
                                                }
                                            });
                                        });
                                    }
                                }
                                ui.add_space(16.0);
                                let any = !self.settings.marked.is_empty();
                                if any && !self.confirm_forget_marks {
                                    hint(ui, "Mail in your Spam folder keeps counting as spam whatever is listed here.");
                                    if ui.button("Forget every mark and example…").clicked() {
                                        self.confirm_forget_marks = true;
                                    }
                                } else if any {
                                    ui.label("Forget every sender and message you marked?");
                                    hint(ui, "The filter goes back to its own checks and the Spam folder. Nothing moves on the server.");
                                    ui.horizontal(|ui| {
                                        if ui.add(egui::Button::new(RichText::new("Forget").color(t.on_accent)).fill(t.danger)).clicked() {
                                            self.settings.marked.clear();
                                            match self.archive.clear_examples() {
                                                Ok(()) => (changed, rescreen) = (true, true),
                                                Err(e) => self.error = Some(e.to_string()),
                                            }
                                            self.confirm_forget_marks = false;
                                        }
                                        if ui.button("Cancel").clicked() {
                                            self.confirm_forget_marks = false;
                                        }
                                    });
                                }
                            }
                            SettingsPage::Encryption => {
                                key_action = encryption::page(ui, t, &key_ids);
                            }
                            SettingsPage::Storage => {
                                vault_request = security::section(ui, t, &mut self.security, mode, locked, self.vault.has_recovery(), job.as_deref());
                                if locked && ui.button("Unlock…").clicked() {
                                    unlock_again = true;
                                }
                            }
                            SettingsPage::Troubleshooting => {
                                let about = "How long messages take to open, and what fails. Never the text, subject or addresses of a message.";
                                if views::switch_row(ui, t, &mut self.settings.diagnostics, "Write a diagnostic log", about).changed() {
                                    diag::set(store::data_dir().ok().filter(|_| self.settings.diagnostics).as_deref());
                                    changed = true;
                                }
                                if let Ok(dir) = store::data_dir() {
                                    ui.add_space(8.0);
                                    hint(ui, &format!("Kept in {}", diag::path(&dir).display()));
                                    let found = diag::path(&dir).exists();
                                    if ui.add_enabled(found, egui::Button::new("Show the log")).on_disabled_hover_text("Nothing written yet").clicked()
                                        && let Err(e) = opener::open(&dir)
                                    {
                                        self.error = Some(format!("Could not open {}: {e}", dir.display()));
                                    }
                                }
                            }
                        }
                    });
                    ui.add_space(8.0);
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                        if !dirty {
                            if ui.button("Done").clicked() {
                                goto = Some(None);
                            }
                            return;
                        }
                        let ready = self.account_edit.as_ref().is_some_and(NewAccount::ready);
                        let button = egui::Button::new(RichText::new("Save changes").color(t.on_accent)).fill(t.accent);
                        save = ui.add_enabled(ready, button).on_disabled_hover_text("User and IMAP server are required").clicked();
                        discard = ui.button("Discard changes").clicked();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| match self.account_edit.as_ref().and_then(|f| f.error.as_deref()) {
                            Some(error) => ui.add(egui::Label::new(RichText::new(error).size(13.0).color(t.danger)).wrap()),
                            None => ui.label(RichText::new("Unsaved changes").color(t.text_dim)),
                        });
                    });
                }));
            });
        });
        if modal.should_close() {
            goto = Some(None);
        }
        if let Some(target) = goto {
            if dirty {
                self.settings_leave = Some(target);
            } else {
                self.leave_settings_page(target);
            }
        }
        if discard {
            self.account_edit = None;
        }
        if save && let Some(form) = self.account_edit.take() {
            match self.save_account(&form) {
                Ok(serial) => {
                    self.settings_page = SettingsPage::Account(serial);
                    self.notice = Some(("Account saved".into(), std::time::Instant::now()));
                }
                Err(e) => self.account_edit = Some(NewAccount { error: Some(e), ..form }),
            }
        }
        if let Some(action) = key_action {
            self.key_action(action);
        }
        match (page_action, self.settings_page) {
            (Some(PageAction::Remove), SettingsPage::Account(serial)) => self.confirm_remove = Some(serial),
            (Some(PageAction::SignIn(provider)), SettingsPage::Account(serial)) => self.sign_in(provider, Some(serial)),
            (Some(PageAction::CancelSignIn), _) => self.cancel_sign_in(),
            (Some(PageAction::Test), SettingsPage::Account(serial)) => self.test_connection(Some(serial)),
            _ => {}
        }
        if add {
            self.form = Some(NewAccount::default());
        }
        self.leave_dialog(ctx, t);
        if changed {
            self.save_settings();
        }
        if rescreen {
            self.rescreen();
        }
        if unlock_again {
            self.unlock_skipped = false;
        }
        if let Some(request) = vault_request {
            self.run_vault(request, ctx);
        }
    }

    /// Goes to another settings page, or closes Settings with `None`, dropping any unsaved account changes.
    fn leave_settings_page(&mut self, target: Option<SettingsPage>) {
        self.account_edit = None;
        if self.signing_in.as_ref().is_some_and(|s| s.editing.is_some()) {
            self.cancel_sign_in();
        }
        match target {
            Some(page) => self.settings_page = page,
            None => self.show_settings = false,
        }
    }

    /// Asks before an account page with unsaved changes is left.
    fn leave_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(target) = self.settings_leave else { return };
        let name = self.account_edit.as_ref().and_then(|f| self.view(f.editing?)).map(|v| v.account.address().to_owned()).unwrap_or_default();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("settings_leave")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(format!("Discard changes to {name}?"));
            ui.label(RichText::new("What you changed on this page has not been saved.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Discard changes").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Keep editing").clicked();
            });
        });
        if confirm {
            self.leave_settings_page(target);
        }
        if confirm || cancel || modal.should_close() {
            self.settings_leave = None;
        }
    }

    /// The command palette: every command and folder, found by typing, run with Enter.
    fn palette_window(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(mut palette) = self.palette.take() else { return };
        let moving = !palette.moving.is_empty();
        let mut entries: Vec<(Entry, String, String)> = if moving { Vec::new() } else { Command::ALL.into_iter().filter(|c| *c != Command::Palette).map(|c| (Entry::Run(c), c.label().to_owned(), c.keys(ctx))).collect() };
        if let Some(view) = palette.moving.first().and_then(|id| self.view(self.serial_of(*id)?)).filter(|_| moving) {
            // Not the folder they are all in already; its whole path, as subfolders can share a name.
            let from: std::collections::HashSet<String> = palette.moving.iter().filter_map(|id| Some(self.archive.locate(*id).ok()??.folder)).collect();
            for (i, _) in views::folder_tree(&view.mailboxes).into_iter().filter(|&(i, _)| !(from.len() == 1 && from.contains(&view.mailboxes[i].name))) {
                let mailbox = &view.mailboxes[i];
                let name = views::folder_path(&mailbox.name, mailbox.role, mailbox.delimiter);
                entries.push((Entry::MoveTo(view.serial, i), name, String::new()));
            }
        }
        for view in self.accounts.iter().filter(|_| !moving) {
            for (i, _) in views::folder_tree(&view.mailboxes) {
                let mailbox = &view.mailboxes[i];
                entries.push((Entry::Folder(view.serial, i), format!("Go to {} › {}", view.account.address(), views::folder_title(&mailbox.name, mailbox.role, mailbox.delimiter)), String::new()));
            }
        }
        entries.retain(|(_, label, _)| Command::matches(label, &palette.query));
        let why = |entry: &Entry| match entry {
            Entry::Run(c) => self.can(*c).err(),
            Entry::Folder(..) | Entry::MoveTo(..) => None,
        };
        let last = entries.len().saturating_sub(1);
        if !palette.moved {
            palette.at = entries.iter().position(|(e, ..)| why(e).is_none()).unwrap_or(0);
        }
        // Only the arrows bring the chosen line into view, so scrolling with the wheel stays put.
        let mut follow = false;
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            palette.at = (palette.at + 1).min(last);
            (palette.moved, follow) = (true, true);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            palette.at = palette.at.saturating_sub(1);
            (palette.moved, follow) = (true, true);
        }
        // A page is the lines the list shows at once.
        let page = (PALETTE_LIST_H / (views::PALETTE_ROW_H + 2.0)).floor() as usize;
        if ctx.input(|i| i.key_pressed(egui::Key::PageDown)) {
            palette.at = (palette.at + page).min(last);
            (palette.moved, follow) = (true, true);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::PageUp)) {
            palette.at = palette.at.saturating_sub(page);
            (palette.moved, follow) = (true, true);
        }
        palette.at = palette.at.min(last);
        let mut chosen = ctx.input(|i| i.key_pressed(egui::Key::Enter)).then_some(palette.at).filter(|at| entries.get(*at).is_some_and(|(e, ..)| why(e).is_none()));
        let id = egui::Id::new("palette");
        let area = egui::Modal::default_area(id).anchor(egui::Align2::CENTER_TOP, vec2(0.0, 90.0));
        let modal = egui::Modal::new(id).area(area).show(ctx, |ui| {
            ui.set_width(PALETTE_WIDTH);
            let before = palette.query.clone();
            // A row of fixed height: the box centres itself in whatever height it is given.
            ui.allocate_ui_with_layout(vec2(PALETTE_WIDTH, theme::CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                views::search_box(ui, t, &mut palette.query, if moving { "Move to the folder…" } else { "Type a command or a folder" }, PALETTE_WIDTH, true);
            });
            if palette.query != before {
                palette.moved = false;
            }
            ui.add_space(8.0);
            if entries.is_empty() {
                ui.label(RichText::new(if moving { "No folder matches." } else { "No command or folder matches." }).color(t.text_dim));
            }
            egui::ScrollArea::vertical().max_height(PALETTE_LIST_H).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, (entry, label, keys)) in entries.iter().enumerate() {
                    let reason = why(entry);
                    let row = views::palette_row(ui, t, label, keys, i == palette.at, reason.is_none());
                    if follow && i == palette.at {
                        row.scroll_to_me(None);
                    }
                    match reason {
                        Some(reason) => drop(row.on_hover_text(reason)),
                        None if row.clicked() => chosen = Some(i),
                        None => {}
                    }
                }
            });
            ui.add_space(6.0);
            let enter = if moving { "Enter to move" } else { "Enter to run" };
            ui.label(RichText::new(format!("↑ ↓ and Page Up/Down to choose  ·  {enter}  ·  Esc to close")).size(12.5).color(t.text_dim));
        });
        if let Some((entry, ..)) = chosen.and_then(|at| entries.get(at)) {
            match *entry {
                Entry::Run(command) => self.run(command),
                Entry::Folder(serial, index) => {
                    self.section = Section::Mail;
                    self.show_suspicious = false;
                    self.select_mailbox(serial, index);
                }
                Entry::MoveTo(serial, index) => self.move_to_folder(palette.moving, serial, index),
            }
        } else if !modal.should_close() {
            self.palette = Some(palette);
        }
    }

    /// Confirms leaving a mailing list, naming where the request goes.
    fn unsubscribe_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some((who, way)) = self.confirm_unsubscribe.clone() else { return };
        let host = way.host();
        let (text, button) = match &way {
            unsubscribe::Method::Page(_) => (format!("The unsubscribe page of {host} opens in your browser. It may ask you to confirm there."), "Open unsubscribe page"),
            _ => (format!("BeaverMail asks {host} to stop sending you this mail. Nothing opens in your browser."), "Unsubscribe"),
        };
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_unsubscribe")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(format!("Unsubscribe from {who}?"));
            ui.label(RichText::new(text).color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new(button).color(t.on_accent)).fill(t.accent)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            match way {
                unsubscribe::Method::OneClick(link) => self.spawn(async move {
                    let result = tokio::task::spawn_blocking(move || remote::one_click(&link)).await.unwrap_or_else(|e| Err(e.to_string()));
                    Event::Unsubscribed(who, result)
                }),
                unsubscribe::Method::Page(link) => ctx.open_url(egui::OpenUrl::new_tab(link)),
                unsubscribe::Method::Mail(..) => {}
            }
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_unsubscribe = None;
        }
    }

    /// Opens the source of the open message in a tab of its own, or shows the one already open.
    fn view_source(&mut self, id: MessageId) {
        let at = match self.tabs.iter().position(|t| t.source && t.message.id == id) {
            Some(at) => at,
            None => {
                let (Some(serial), Some(message)) = (self.serial_of(id), self.summary_of(id).cloned()) else { return };
                self.tabs.push(tabs::Tab { serial, message, role: self.role_of(id), pinned: true, source: true });
                self.tabs.len() - 1
            }
        };
        self.activate(at);
    }

    /// A source tab is shown.
    fn showing_source(&self) -> bool {
        self.in_tab && self.source_view.as_ref().is_some_and(|v| Some(v.id) == self.message)
    }

    /// The source tab: the message as the server holds it, every header and part, to read, search or
    /// copy. The keys scroll it as they scroll a message, Left and Right along its long lines.
    fn source_page(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let scroll = self.reader_scroll.take();
        let typing = ui.ctx().egui_wants_keyboard_input();
        let Some(view) = &mut self.source_view else { return };
        let mut copy = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(t.surface).inner_margin(egui::Margin::symmetric(28, 20))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading(if view.subject.trim().is_empty() { "(no subject)" } else { view.subject.as_str() });
                    let about = if view.size > 0 { format!("Message source  ·  {}", attachments::human_size(view.size as u64)) } else { "Message source".to_owned() };
                    ui.label(RichText::new(about).size(13.0).color(t.text_dim));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ready = matches!(view.lines, Some(Ok(_)));
                    copy = ui.add_enabled(ready, egui::Button::new("Copy source")).on_hover_text("Copy every header and part, as the server holds them").clicked();
                    if ui.add_enabled(ready, egui::Button::new("Find…")).on_hover_text(Command::FindInSource.tip(ui.ctx())).clicked() {
                        view.find.get_or_insert_with(Find::default).focus = true;
                    }
                });
            });
            ui.add_space(12.0);
            let lines: &[String] = match &view.lines {
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("Loading the message…").color(t.text_dim));
                    });
                    return;
                }
                Some(Err(why)) => {
                    ui.label(RichText::new(format!("The message could not be read: {why}")).color(t.danger));
                    return;
                }
                Some(Ok(lines)) => lines,
            };
            if let Some(find) = &mut view.find {
                if find_bar(ui, t, find, lines) {
                    view.find = None;
                }
                ui.add_space(8.0);
            }
            let font = egui::FontId::monospace(13.0);
            let (row, glyph) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
            // The match to show: its line a third of the way down, its column in sight.
            let jump = view.find.as_mut().and_then(|f| if std::mem::take(&mut f.jump) { f.hits.get(f.at).copied() } else { None });
            let height = ui.available_height() - 16.0;
            let hits: &[(usize, usize, usize)] = view.find.as_ref().map_or(&[], |f| &f.hits);
            let current = view.find.as_ref().and_then(|f| f.hits.get(f.at).copied());
            egui::Frame::new().fill(t.input).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                // Rows drawn only as they scroll into view: a message with attachments is megabytes of text.
                // No gap between lines, set before the area works out its height from it.
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut area = egui::ScrollArea::both().id_salt(("source", view.id.0)).auto_shrink([false, false]);
                if let Some((line, start, _)) = jump {
                    let column = lines[line][..start].chars().count() as f32 * glyph;
                    area = area.vertical_scroll_offset((line as f32 * row - height / 3.0).max(0.0)).horizontal_scroll_offset((column - 120.0).max(0.0));
                }
                area.show_rows(ui, row, lines.len(), |ui, range| {
                    let page = (ui.clip_rect().height() - 48.0).max(48.0);
                    let sideways = if typing { 0.0 } else { ui.input(|i| f32::from(u8::from(i.key_pressed(egui::Key::ArrowLeft))) - f32::from(u8::from(i.key_pressed(egui::Key::ArrowRight)))) };
                    let down = match scroll {
                        Some(Scroll::Lines(n)) => -48.0 * n,
                        Some(Scroll::Pages(n)) => -page * n,
                        Some(Scroll::Edge(end)) => if end { -1e9 } else { 1e9 },
                        None => 0.0,
                    };
                    if down != 0.0 || sideways != 0.0 {
                        ui.scroll_with_delta(vec2(sideways * 96.0, down));
                    }
                    let first = hits.partition_point(|h| h.0 < range.start);
                    let mut hits = hits[first..].iter().peekable();
                    for (at, line) in lines[range.clone()].iter().enumerate().map(|(i, l)| (range.start + i, l)) {
                        let mut job = egui::text::LayoutJob::default();
                        let plain = egui::TextFormat::simple(font.clone(), t.text);
                        let mut from = 0;
                        while let Some(&&(l, start, end)) = hits.peek().filter(|h| h.0 == at) {
                            hits.next();
                            job.append(&line[from..start], 0.0, plain.clone());
                            let on = current == Some((l, start, end));
                            let (fill, color) = if on { (t.accent, t.on_accent) } else { (t.accent.gamma_multiply(0.3), t.text) };
                            job.append(&line[start..end], 0.0, egui::TextFormat { background: fill, ..egui::TextFormat::simple(font.clone(), color) });
                            from = end;
                        }
                        job.append(&line[from..], 0.0, plain);
                        ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
                    }
                });
            });
        });
        if copy && let Some(Ok(lines)) = &view.lines {
            ui.ctx().copy_text(lines.join("\n"));
            self.notice = Some(("Source copied".into(), std::time::Instant::now()));
        }
    }

    /// Confirms blocking the sender of a message: the same as marking it as spam, asked for on any message.
    fn block_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(id) = self.confirm_block else { return };
        let Some(key) = self.summary_of(id).map(|m| screen::sender_key(&m.address)) else {
            self.confirm_block = None;
            return;
        };
        let who = key.trim_start_matches('@').to_owned();
        let later = if self.settings.hold_suspicious {
            "New mail from them is held apart from the Inbox."
        } else {
            "New mail from them still reaches the Inbox while holding suspicious mail is off in Settings."
        };
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_block")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(format!("Block {who}?"));
            ui.label(RichText::new(format!("This message and their mail in this folder move to Spam. {later} Choose Not spam on any of their messages to undo it.")).color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Block sender").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            // The message itself too: from a tab or a search it may not be in the list shown.
            let mut ids: Vec<MessageId> = self.messages.iter().filter(|m| screen::sender_key(&m.address) == key).map(|m| m.id).collect();
            if !ids.contains(&id) {
                ids.push(id);
            }
            self.move_to_spam(&ids);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_block = None;
        }
    }

    /// Whether the message can go encrypted, worked out again when its sender or recipients change;
    /// while the user has not chosen, it is encrypted whenever it can be.
    fn check_encryption(&mut self, compose: &mut compose::Compose) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (compose.from, compose.recipients()).hash(&mut hasher);
        let state = hasher.finish();
        if self.encryption_checked.as_ref().is_none_or(|(s, _)| *s != state) {
            let can = self.seal_keys(compose).map(|_| ());
            self.encryption_checked = Some((state, can));
        }
        if let Some((_, can)) = &self.encryption_checked {
            compose.can_encrypt = can.clone();
            if compose.encrypt_auto {
                compose.encrypt = can.is_ok();
            }
        }
    }

    /// The keys to encrypt the message with: the sender's own (signs it, and keeps a readable copy) and
    /// every recipient's, or why it cannot be encrypted.
    fn seal_keys(&mut self, compose: &compose::Compose) -> Result<beavermail_core::Seal, String> {
        let address = self.view(compose.from).map(|v| v.account.address().to_lowercase()).ok_or("The sending account is gone")?;
        let recipients = compose.recipients();
        if recipients.is_empty() {
            return Err(String::new());
        }
        let others: Vec<(String, bool)> = recipients.into_iter().filter(|(r, _)| *r != address).collect();
        // OpenPGP first, the way most people who encrypt write; S/MIME when everyone has a certificate.
        let openpgp = (|| {
            let secret = self.own_key(&address).ok_or_else(|| format!("Turn on encryption for {address} in Settings > Encryption first"))?;
            let own = beavermail_crypto::public_key(&secret).ok_or("Your key could not be read")?;
            let (mut to, mut bcc) = (vec![own], Vec::new());
            for (recipient, blind) in &others {
                let key = self.archive.peer_key(recipient).ok().flatten().ok_or_else(|| format!("No key for {recipient} yet. It comes with their mail, once they use a program that encrypts."))?;
                if *blind { bcc.push(key) } else { to.push(key) }
            }
            Ok(beavermail_core::Seal::OpenPgp { secret, to, bcc })
        })();
        if openpgp.is_ok() {
            return openpgp;
        }
        let smime = (|| {
            let own = self.own_certificate(&address).ok_or(())?;
            let to = others.iter().map(|(r, _)| self.archive.peer_cert(r).ok().flatten().ok_or(())).collect::<Result<Vec<_>, ()>>()?;
            Ok::<_, ()>(beavermail_core::Seal::Smime { own, to })
        })();
        smime.or(openpgp)
    }

    /// Encryption to the user alone, for a draft kept on the server: with their OpenPGP key, else their certificate.
    fn seal_to_self(&mut self, address: &str) -> Option<beavermail_core::Seal> {
        if let Some(secret) = self.own_key(address) {
            let public = beavermail_crypto::public_key(&secret)?;
            return Some(beavermail_core::Seal::OpenPgp { secret, to: vec![public], bcc: Vec::new() });
        }
        self.own_certificate(address).map(|own| beavermail_core::Seal::Smime { own, to: Vec::new() })
    }

    /// A key or certificate of the user's, read from the vault once.
    fn key_file(&mut self, path: std::path::PathBuf) -> Option<Vec<u8>> {
        if !self.key_files.contains_key(&path) {
            let data = self.vault.read(&path).ok().flatten();
            self.key_files.insert(path.clone(), data);
        }
        self.key_files.get(&path).cloned().flatten()
    }

    /// Syncs the address book at `url`, or every one, in the background; one already syncing is left
    /// to finish.
    fn sync_contacts(&mut self, url: Option<&str>) {
        let books: Vec<beavermail_core::AddressBook> = self.settings.address_books.iter().filter(|b| url.is_none_or(|u| u == b.url)).cloned().collect();
        for book in books {
            if !self.contacts_syncing.insert(book.url.clone()) {
                continue;
            }
            let password = match store::book_password(&book) {
                Ok(password) => password,
                Err(e) => {
                    self.contacts_syncing.remove(&book.url);
                    self.error = Some(format!("Address book {}: {e}", book.name));
                    continue;
                }
            };
            let archive = self.archive.clone();
            self.spawn(async move {
                let url = book.url.clone();
                let result = tokio::task::spawn_blocking(move || beavermail_carddav::sync(&beavermail_carddav::Book::open(&book.url, &book.user, &password), &archive))
                    .await
                    .unwrap_or_else(|e| Err(MailError::Backend(e.to_string())));
                Event::ContactsSynced(url, result)
            });
        }
    }

    /// Asks the server in the "Add address book" dialog for its address books.
    fn find_books(&mut self) {
        let Some(form) = self.add_book.as_mut() else { return };
        let (server, user, password) = (form.server.trim().to_owned(), form.user.trim().to_owned(), form.password.clone());
        (form.finding, form.found) = (true, None);
        self.spawn(async move {
            let found = tokio::task::spawn_blocking(move || beavermail_carddav::Book::discover_all(&server, &user, &password).map_err(|e| e.to_string())).await.unwrap_or_else(|e| Err(e.to_string()));
            Event::BooksFound(found)
        });
    }

    /// Syncs the calendars (or the one at `url`) in the background; the one on this computer has no server.
    fn sync_calendars(&mut self, url: Option<&str>) {
        let all: Vec<beavermail_core::CalendarSource> = self.settings.calendars.iter().filter(|c| !c.is_local() && url.is_none_or(|u| u == c.url)).cloned().collect();
        for calendar in all {
            if !self.calendars_syncing.insert(calendar.url.clone()) {
                continue;
            }
            let password = match store::calendar_password(&calendar) {
                Ok(password) => password,
                Err(e) => {
                    self.calendars_syncing.remove(&calendar.url);
                    self.error = Some(format!("Calendar {}: {e}", calendar.name));
                    continue;
                }
            };
            let archive = self.archive.clone();
            self.spawn(async move {
                let url = calendar.url.clone();
                let open = beavermail_carddav::calendar::Calendar::open(&calendar.url, &calendar.user, &password);
                let result = tokio::task::spawn_blocking(move || beavermail_carddav::calendar::sync(&open, &archive)).await.unwrap_or_else(|e| Err(MailError::Backend(e.to_string())));
                Event::CalendarsSynced(url, result)
            });
        }
    }

    /// Asks the server in the "Add calendar" dialog for its calendars.
    fn find_calendars(&mut self) {
        let Some(form) = self.add_calendar.as_mut() else { return };
        let (server, user, password) = (form.server.trim().to_owned(), form.user.trim().to_owned(), form.password.clone());
        (form.finding, form.found) = (true, None);
        self.spawn(async move {
            let found = tokio::task::spawn_blocking(move || beavermail_carddav::calendar::Calendar::discover_all(&server, &user, &password).map_err(|e| e.to_string())).await.unwrap_or_else(|e| Err(e.to_string()));
            Event::CalendarsFound(found)
        });
    }

    /// The calendar on this computer is always there: invitations accepted without a server go to it.
    fn ensure_local_calendar(&mut self) {
        if self.settings.calendars.iter().any(beavermail_core::CalendarSource::is_local) {
            return;
        }
        let default = !self.settings.calendars.iter().any(|c| c.default);
        let local = beavermail_core::CalendarSource { name: "On this computer".into(), url: beavermail_core::CalendarSource::LOCAL.into(), user: String::new(), color: String::new(), shown: true, default, read_only: false };
        self.settings.calendars.insert(0, local);
        self.save_settings();
    }

    fn calendar_action(&mut self, action: calendars::CalendarAction) {
        use calendars::CalendarAction as A;
        match action {
            A::Add => {
                // Most often the server and login of a mail account: offered filled in, found on Find.
                let account = self.accounts.first().map(|v| v.account.clone());
                let user = account.as_ref().map(|a| if a.email.is_empty() { a.user.clone() } else { a.email.clone() }).unwrap_or_default();
                let server = user.rsplit_once('@').map(|(_, d)| d.to_owned()).unwrap_or_default();
                let password = account.as_ref().and_then(|a| store::password(a).ok()).unwrap_or_default();
                self.add_calendar = Some(books::AddBook { server, user, password, focus_first: true, ..Default::default() });
            }
            A::Added(chosen) => {
                let Some(form) = self.add_calendar.take() else { return };
                for found in chosen {
                    let calendar = beavermail_core::CalendarSource { name: found.name, url: found.url, user: form.user.trim().to_owned(), color: found.color, shown: true, default: false, read_only: !found.writable };
                    if let Err(e) = store::set_calendar_password(&calendar, &form.password) {
                        self.error = Some(e);
                        return;
                    }
                    self.settings.calendars.push(calendar);
                }
                self.save_settings();
                self.calendar_version += 1;
                self.sync_calendars(None);
            }
            A::Toggle(url) => {
                if let Some(c) = self.settings.calendars.iter_mut().find(|c| c.url == url) {
                    c.shown = !c.shown;
                }
                self.save_settings();
            }
            A::MakeDefault(url) => {
                for c in &mut self.settings.calendars {
                    c.default = c.url == url;
                }
                self.save_settings();
            }
            A::SyncNow => self.sync_calendars(None),
            A::AskRemove(url) => self.confirm_calendar_remove = Some(url),
            A::Fold(key) => {
                if !self.settings.folded.remove(&key) {
                    self.settings.folded.insert(key);
                }
                self.save_settings();
            }
            A::Pick(day) => {
                self.calendar_day = day;
                self.calendar_morning = true;
            }
            A::Month(delta) => {
                let month = self.calendar_month;
                self.calendar_month = if delta < 0 { month - chrono::Months::new(1) } else { month + chrono::Months::new(1) };
            }
        }
    }

    /// No longer syncs the calendar at `url`: its events leave this computer, nothing changes on the server.
    fn remove_calendar(&mut self, url: &str) {
        let Some(at) = self.settings.calendars.iter().position(|c| c.url == url) else { return };
        let calendar = self.settings.calendars.remove(at);
        if let Err(e) = self.archive.forget_calendar(&calendar.url) {
            self.error = Some(format!("Could not forget the calendar's events: {e}"));
        }
        if !self.settings.calendars.iter().any(|c| c.user == calendar.user && books::host(&c.url) == books::host(&calendar.url)) {
            let _ = store::forget_calendar_password(&calendar);
        }
        if calendar.default
            && let Some(next) = self.settings.calendars.iter_mut().find(|c| !c.read_only)
        {
            next.default = true;
        }
        self.save_settings();
        self.calendar_version += 1;
        self.calendar_open = None;
    }

    fn calendar_remove_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(url) = self.confirm_calendar_remove.clone() else { return };
        let name = self.settings.calendars.iter().find(|c| c.url == url).map(|c| c.name.clone()).unwrap_or_default();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_calendar_remove")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.85);
            ui.heading(format!("Stop syncing {name}?"));
            ui.label(RichText::new("Its events leave BeaverMail. Nothing changes on the server.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Stop syncing").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.remove_calendar(&url);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_calendar_remove = None;
        }
    }

    /// Opens the dialog for a new event on `day`: at `minutes` past midnight when an hour was clicked,
    /// else at the next full hour today or nine o'clock another day; in the default calendar.
    fn new_event(&mut self, day: chrono::NaiveDate, minutes: Option<u32>) {
        self.ensure_local_calendar();
        let now = chrono::Local::now();
        let minutes = minutes.unwrap_or(if day == now.date_naive() { (chrono::Timelike::hour(&now) + 1).min(23) * 60 } else { 9 * 60 });
        let calendar = self.settings.calendars.iter().find(|c| c.default && !c.read_only).or_else(|| self.settings.calendars.iter().find(|c| c.is_local())).map(|c| c.url.clone()).unwrap_or_default();
        self.event_form = Some(event_form::EventForm::new(day, minutes, &calendar));
    }

    /// Whether an event can be changed and deleted here, or why not. Only the user's own: not in a
    /// read-only calendar, not an invitation from someone else, not a meeting whose people would not be
    /// told; a repeating one can only be deleted (all of it) for now.
    fn event_can(&self, item: &calview::Item) -> (Result<(), &'static str>, Result<(), &'static str>) {
        let event = &item.occurrence.event;
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_lowercase()).collect();
        if self.settings.calendars.iter().find(|c| c.url == item.calendar).is_none_or(|c| c.read_only) {
            let why = "A read-only calendar: it cannot be changed here";
            return (Err(why), Err(why));
        }
        if event.organizer.as_ref().is_some_and(|o| !own.contains(&o.email)) {
            let why = "An invitation: its organiser changes it. Answer it from its message";
            return (Err(why), Err(why));
        }
        if event.attendees.iter().any(|a| !own.contains(&a.email)) {
            let why = "A meeting with people invited: they would not be told of a change made here";
            return (Err(why), Err(why));
        }
        if !event.rules.is_empty() || !event.rdates.is_empty() || event.recurrence_id.is_some() {
            return (Err("A repeating event: it can only be deleted here, all of it, for now"), Ok(()));
        }
        (Ok(()), Ok(()))
    }

    /// The new-event dialog and the question before deleting one.
    fn event_dialogs(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let targets: Vec<(String, String)> = self.settings.calendars.iter().filter(|c| !c.read_only).map(|c| (c.url.clone(), c.name.clone())).collect();
        if let Some((form, draft)) = event_form::dialog(ctx, t, &mut self.event_form, &targets) {
            let (uid, old) = match &form.uid {
                Some(uid) => (uid.clone(), self.archive.events_with_uid(uid).unwrap_or_default().into_iter().next().map(|k| k.ics)),
                None => (beavermail_carddav::new_uid(), None),
            };
            if form.uid.is_some() && old.is_none() {
                self.error = Some("The event is no longer here: it was deleted meanwhile".into());
                return;
            }
            let ics = beavermail_carddav::ical::written(old.as_deref(), &uid, &draft, chrono::Utc::now());
            self.put_event(&uid, ics, Some(&form.calendar));
            // What the details showed is the old version.
            self.calendar_open = None;
            self.notice = Some((if form.uid.is_some() { "Changes saved" } else { "Event added" }.into(), std::time::Instant::now()));
        }
        let Some(uid) = self.confirm_event_delete.clone() else { return };
        let kept = self.archive.events_with_uid(&uid).unwrap_or_default().into_iter().next();
        let event = kept.as_ref().and_then(|k| beavermail_carddav::ical::parse(&k.ics).events.into_iter().find(|e| e.recurrence_id.is_none()));
        let title = event.as_ref().map(|e| e.summary.trim().to_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "this event".into());
        let repeats = event.as_ref().is_some_and(|e| !e.rules.is_empty() || !e.rdates.is_empty());
        let local = kept.as_ref().is_some_and(|k| k.calendar == beavermail_core::CalendarSource::LOCAL);
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_event_delete")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.85);
            ui.heading(format!("Delete {title}?"));
            let what = match (repeats, local) {
                (true, true) => "Every time it repeats goes.",
                (true, false) => "Every time it repeats goes, on the server too.",
                (false, true) => "It leaves your calendar.",
                (false, false) => "It leaves your calendar, on the server too.",
            };
            ui.label(RichText::new(what).color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Delete event").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.remove_event(&uid);
            self.calendar_open = None;
            self.notice = Some(("Event deleted".into(), std::time::Instant::now()));
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_event_delete = None;
        }
    }

    /// What the calendars here say about the event of `ics`: whether one has it, the answer given, and
    /// what else happens at that time.
    fn invite_info(&mut self, ics: &str) -> (beavermail_carddav::ical::Calendar, calview::InviteInfo) {
        let calendar = beavermail_carddav::ical::parse(ics);
        let mut info = calview::InviteInfo::default();
        let Some(event) = calendar.events.iter().find(|e| e.recurrence_id.is_none()).or(calendar.events.first()).cloned() else { return (calendar, info) };
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_lowercase()).collect();
        info.mine = calendar.method == "REPLY" && event.attendees.first().is_some_and(|a| own.contains(&a.email));
        for stored in self.archive.events_with_uid(&event.uid).unwrap_or_default() {
            info.in_calendar = true;
            let kept = beavermail_carddav::ical::parse(&stored.ics);
            info.answered = kept.events.iter().flat_map(|e| &e.attendees).find(|a| own.contains(&a.email)).map(|a| a.partstat.clone()).or(info.answered);
        }
        if let Some(start) = event.start.as_ref().and_then(beavermail_carddav::ical::When::utc) {
            let end = event.end.as_ref().and_then(beavermail_carddav::ical::When::utc).or_else(|| event.duration.map(|d| start + chrono::Duration::seconds(d))).unwrap_or(start);
            let day = start.with_timezone(&chrono::Local).date_naive();
            let days = (end - start).num_days() + 2;
            let busy = event.start.as_ref().is_some_and(|s| !s.date_only);
            info.conflicts = self
                .calendar_items(day, days)
                .iter()
                .filter(|i| busy && !i.occurrence.all_day && i.occurrence.uid != event.uid && i.occurrence.start < end && i.occurrence.end > start)
                .map(|i| i.occurrence.event.summary.clone())
                .collect();
        }
        (calendar, info)
    }

    /// Answers the invitation message `id` carries: the answer goes to the organiser through the Outbox
    /// (so it waits for the server if need be), and the event goes into a calendar here with it, or leaves
    /// them on Decline; a cancellation is removed, an event sent to add is added.
    fn answer_invitation(&mut self, id: MessageId, choice: calview::Invite) {
        let Some(ics) = self.body_of(id).and_then(|b| b.data.calendar.clone()) else { return };
        let Some(serial) = self.serial_of(id) else { return };
        let Some(account) = self.view(serial).map(|v| v.account.clone()) else { return };
        let calendar = beavermail_carddav::ical::parse(&ics);
        let Some(event) = calendar.events.iter().find(|e| e.recurrence_id.is_none()).or(calendar.events.first()).cloned() else { return };
        let me = account.address().to_lowercase();
        match choice {
            calview::Invite::Answer(partstat) => {
                if let Some(organizer) = event.organizer.as_ref().filter(|o| !o.email.is_empty())
                    && let Some(reply) = beavermail_carddav::ical::reply(&ics, &me, account.sender_name().unwrap_or_default(), partstat, chrono::Utc::now())
                {
                    let (word, verb) = match partstat {
                        "ACCEPTED" => ("Accepted", "accepted"),
                        "TENTATIVE" => ("Tentative", "tentatively accepted"),
                        _ => ("Declined", "declined"),
                    };
                    let name = account.sender_name().map_or_else(|| me.clone(), str::to_owned);
                    let draft = drafts::DraftData {
                        id: drafts::new_id(),
                        title: "Reply".into(),
                        to: beavermail_core::address_text(&organizer.name, &organizer.email),
                        subject: format!("{word}: {}", event.summary.trim()),
                        body: format!("{name} has {verb} this invitation: {}", event.summary.trim()),
                        updated: chrono::Local::now().timestamp(),
                        queued: true,
                        calendar: Some(reply),
                        ..Default::default()
                    };
                    match drafts::save(&self.vault, &account.key(), &draft) {
                        Ok(()) => {
                            self.count_outbox();
                            self.send_outbox(serial);
                        }
                        Err(e) => {
                            self.error = Some(format!("Could not send the answer: {e}"));
                            return;
                        }
                    }
                }
                // Declined too, it stays in the calendar with the answer (as servers keep it), so the
                // answer is remembered and can be changed; the views leave it out.
                self.put_event(&event.uid, beavermail_carddav::ical::with_answer(&ics, &me, partstat), None);
                let said = match partstat {
                    "ACCEPTED" => "Accepted: the organiser is told, and it is in your calendar",
                    "TENTATIVE" => "Answered maybe: the organiser is told, and it is in your calendar as tentative",
                    _ => "Declined: the organiser is told, and it leaves your calendar views",
                };
                self.notice = Some((said.into(), std::time::Instant::now()));
            }
            calview::Invite::Remove => self.remove_event(&event.uid),
            calview::Invite::Add => self.put_event(&event.uid, beavermail_carddav::ical::as_kept(&ics), None),
            calview::Invite::Show => self.show_event(&event.uid),
            calview::Invite::Open(_) => {}
        }
    }

    /// Shows the event with this UID in the Calendar section: its week, with its details open.
    fn show_event(&mut self, uid: &str) {
        let Some(kept) = self.archive.events_with_uid(uid).unwrap_or_default().into_iter().next() else { return };
        let events = beavermail_carddav::ical::parse(&kept.ics).events;
        let Some(start) = events.iter().find(|e| e.recurrence_id.is_none()).and_then(|e| e.start.as_ref()?.utc()) else { return };
        let day = start.with_timezone(&chrono::Local).date_naive();
        if let Some(calendar) = self.settings.calendars.iter_mut().find(|c| c.url == kept.calendar && !c.shown) {
            calendar.shown = true; // a hidden calendar would hide it
            self.save_settings();
        }
        (self.section, self.calendar_view, self.calendar_day, self.calendar_morning) = (Section::Calendar, calview::View::Week, day, true);
        self.calendar_month = day.with_day(1).unwrap_or(day);
        let (from, days) = calview::range(calview::View::Week, day);
        self.calendar_open = self.calendar_items(from, days).iter().find(|i| i.occurrence.uid == uid).cloned();
    }

    /// Keeps an event in the calendar that has it already (an update), else in `into` when given and
    /// writable, else in the default calendar (the one on this computer when there is no other): on the
    /// server first, then here.
    fn put_event(&mut self, uid: &str, ics: String, into: Option<&str>) {
        self.ensure_local_calendar();
        let span = beavermail_carddav::calendar::span(&ics);
        let (calendar, href, etag) = match self.archive.events_with_uid(uid).unwrap_or_default().into_iter().next() {
            Some(kept) => (kept.calendar, kept.href, Some(kept.etag)),
            None => {
                let chosen = into.and_then(|url| self.settings.calendars.iter().find(|c| c.url == url && !c.read_only));
                let target = chosen.or_else(|| self.settings.calendars.iter().find(|c| c.default && !c.read_only)).or_else(|| self.settings.calendars.iter().find(|c| c.is_local())).cloned();
                let Some(target) = target else { return };
                let href = if target.is_local() { format!("{uid}.ics") } else { beavermail_carddav::calendar::Calendar::open(&target.url, "", "").new_href(uid) };
                (target.url, href, None)
            }
        };
        let Some(source) = self.settings.calendars.iter().find(|c| c.url == calendar).cloned() else { return };
        if source.is_local() {
            if let Err(e) = self.archive.put_event(&calendar, &href, "", &ics, &span) {
                self.error = Some(format!("Could not keep the event: {e}"));
            }
            self.calendar_version += 1;
            return;
        }
        let password = match store::calendar_password(&source) {
            Ok(p) => p,
            Err(e) => return self.error = Some(format!("Calendar {}: {e}", source.name)),
        };
        let archive = self.archive.clone();
        self.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let open = beavermail_carddav::calendar::Calendar::open(&source.url, &source.user, &password);
                match open.put(&href, &ics, etag.as_deref().filter(|e| !e.is_empty()))? {
                    // Without the server's new version, the next sync reads it again.
                    beavermail_carddav::Written::Done(etag) => archive.put_event(&source.url, &href, &etag.unwrap_or_default(), &ics, &span),
                    beavermail_carddav::Written::Conflict => Err(MailError::Backend("the event changed on the server meanwhile; try again after the next sync".into())),
                }
            })
            .await
            .unwrap_or_else(|e| Err(MailError::Backend(e.to_string())));
            Event::CalendarWritten(result)
        });
    }

    /// Takes the event with this UID out of every calendar here that has it, on the server too.
    fn remove_event(&mut self, uid: &str) {
        for kept in self.archive.events_with_uid(uid).unwrap_or_default() {
            let Some(source) = self.settings.calendars.iter().find(|c| c.url == kept.calendar).cloned() else { continue };
            if source.is_local() {
                let _ = self.archive.forget_event(&kept.calendar, &kept.href);
                self.calendar_version += 1;
                continue;
            }
            let Ok(password) = store::calendar_password(&source) else { continue };
            let archive = self.archive.clone();
            self.spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let open = beavermail_carddav::calendar::Calendar::open(&source.url, &source.user, &password);
                    match open.delete(&kept.href, &kept.etag)? {
                        beavermail_carddav::Written::Done(_) => archive.forget_event(&kept.calendar, &kept.href),
                        beavermail_carddav::Written::Conflict => Err(MailError::Backend("the event changed on the server meanwhile; try again after the next sync".into())),
                    }
                })
                .await
                .unwrap_or_else(|e| Err(MailError::Backend(e.to_string())));
                Event::CalendarWritten(result)
            });
        }
    }

    /// The occurrences of the calendars shown in `days` days from `from`, worked out once per range
    /// and kept while nothing changes.
    fn calendar_items(&mut self, from: chrono::NaiveDate, days: i64) -> Arc<Vec<calview::Item>> {
        let shown: Vec<(usize, &beavermail_core::CalendarSource)> = self.settings.calendars.iter().enumerate().filter(|(_, c)| c.shown).collect();
        let key = (from, days, shown.iter().map(|(_, c)| c.url.as_str()).collect::<Vec<_>>().join(" "), self.calendar_version);
        if let Some((_, items)) = self.calendar_cache.iter().find(|(k, _)| *k == key) {
            return items.clone();
        }
        let (start, end) = (calview::midnight(from), calview::midnight(from + chrono::Duration::days(days)));
        let urls: Vec<String> = shown.iter().map(|(_, c)| c.url.clone()).collect();
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_lowercase()).collect();
        let mut items = Vec::new();
        for stored in self.archive.events_between(&urls, start.timestamp(), end.timestamp()).unwrap_or_default() {
            let Some((at, calendar)) = shown.iter().find(|(_, c)| c.url == stored.calendar) else { continue };
            let color = calendars::color(calendar, *at);
            let events = beavermail_carddav::ical::parse(&stored.ics).events;
            // An invitation declined stays in its calendar with the answer, out of sight.
            if events.iter().any(|e| e.attendees.iter().any(|a| a.partstat == "DECLINED" && own.contains(&a.email))) {
                continue;
            }
            items.extend(beavermail_carddav::calendar::occurrences(&events, start, end).into_iter().map(|occurrence| calview::Item { occurrence, calendar: stored.calendar.clone(), color }));
        }
        items.sort_by_key(|i| (i.occurrence.start, i.occurrence.end));
        let items = Arc::new(items);
        self.calendar_cache.retain(|(k, _)| k.3 == key.3);
        self.calendar_cache.truncate(3);
        self.calendar_cache.push((key, items.clone()));
        items
    }

    /// The Calendar section, laid out as Mail: the month and the calendars on the left, the view in the
    /// middle, an event's details on the right.
    fn calendar_section(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        self.ensure_local_calendar();
        let today = chrono::Local::now().date_naive();
        let day_before = self.calendar_day;
        let typing = ui.ctx().egui_wants_keyboard_input() || egui::Popup::is_any_open(ui.ctx());
        let mut action = None;
        // The keys of the section: T today, left and right for the previous and next range, W, M and A for the views.
        if !typing && self.add_calendar.is_none() && self.confirm_calendar_remove.is_none() && self.event_form.is_none() && self.confirm_event_delete.is_none() && self.palette.is_none() && !self.show_settings {
            let key = |k| ui.input(|i| i.key_pressed(k) && !i.modifiers.command && !i.modifiers.alt);
            let step = match self.calendar_view {
                calview::View::Week => 7,
                calview::View::Month => 0,
                calview::View::Agenda => 30,
            };
            let shift = |day: chrono::NaiveDate, back: bool| match (step, back) {
                (0, false) => day + chrono::Months::new(1),
                (0, true) => day - chrono::Months::new(1),
                (n, false) => day + chrono::Duration::days(n),
                (n, true) => day - chrono::Duration::days(n),
            };
            if key(egui::Key::T) {
                self.calendar_day = today;
                self.calendar_morning = true;
            }
            if key(egui::Key::N) {
                self.new_event(self.calendar_day, None);
            }
            if key(egui::Key::ArrowLeft) {
                self.calendar_day = shift(self.calendar_day, true);
            }
            if key(egui::Key::ArrowRight) {
                self.calendar_day = shift(self.calendar_day, false);
            }
            for (k, view) in [(egui::Key::W, calview::View::Week), (egui::Key::M, calview::View::Month), (egui::Key::A, calview::View::Agenda)] {
                if key(k) {
                    self.calendar_view = view;
                    self.calendar_morning = true;
                }
            }
            if key(egui::Key::Escape) {
                self.calendar_open = None;
            }
        }
        let mini_from = calendars::week_start(self.calendar_month);
        let busy: std::collections::HashSet<chrono::NaiveDate> = {
            let items = self.calendar_items(mini_from, 42);
            (0..42).map(|d| mini_from + chrono::Duration::days(d)).filter(|day| items.iter().any(|i| i.on(*day))).collect()
        };
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::same(12));
        egui::Panel::left("calendars").frame(frame).default_size(260.0).size_range(230.0..=340.0).show(ui, |ui| {
            if let Some(a) = calendars::mini_month(ui, t, self.calendar_month, self.calendar_day, today, &busy) {
                action = Some(a);
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                let add = egui::Button::image_and_text(Icon::Plus.image(icons::SIZE, t.text_dim), "Add calendar");
                if ui.add_sized([ui.available_width(), theme::CONTROL_H], add).on_hover_text("See your events from a CalDAV server").clicked() {
                    action = Some(calendars::CalendarAction::Add);
                }
                ui.add_space(4.0);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        theme::dense(ui, self.settings.density);
                        if let Some(a) = calendars::section(ui, t, &self.settings.calendars, &self.calendars_syncing, &self.settings.folded) {
                            action = Some(a);
                        }
                    });
                });
            });
        });
        let mut url = None;
        if let Some(item) = self.calendar_open.clone() {
            let name = self.settings.calendars.iter().find(|c| c.url == item.calendar).map(|c| c.name.clone()).unwrap_or_default();
            let can = self.event_can(&item);
            let (mut close, mut act) = (false, None);
            let frame = egui::Frame::new().fill(t.surface).inner_margin(egui::Margin::same(24));
            egui::Panel::right("event").frame(frame).default_size(380.0).size_range(320.0..=560.0).show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    url = calview::details(ui, t, &item, &name, can, &mut close, &mut act);
                });
            });
            if close {
                self.calendar_open = None;
            }
            match act {
                Some(calview::EventAction::Edit) => self.event_form = Some(event_form::EventForm::editing(&item)),
                Some(calview::EventAction::Delete) => self.confirm_event_delete = Some(item.occurrence.uid.clone()),
                None => {}
            }
        }
        let (from, days) = calview::range(self.calendar_view, self.calendar_day);
        let items = self.calendar_items(from, days);
        let (mut view_action, mut new_event) = (None, false);
        let frame = egui::Frame::new().fill(t.surface).inner_margin(egui::Margin { left: 16, right: 16, top: 12, bottom: 8 });
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            let (title, week) = calview::title(self.calendar_view, self.calendar_day);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(22.0).family(theme::bold()).color(t.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let new = egui::Button::image_and_text(Icon::Plus.image(icons::SIZE, t.on_accent), RichText::new("New event").color(t.on_accent)).fill(t.accent);
                    if ui.add(new).on_hover_text("Add an event (N)").clicked() {
                        new_event = true;
                    }
                    ui.add_space(8.0);
                    if ui.button("Today").on_hover_text("Back to today (T)").clicked() {
                        self.calendar_day = today;
                        self.calendar_morning = true;
                    }
                    for (view, name, k) in [(calview::View::Agenda, "Agenda", "A"), (calview::View::Month, "Month", "M"), (calview::View::Week, "Week", "W")] {
                        if ui.selectable_label(self.calendar_view == view, name).on_hover_text(format!("{name} ({k})")).clicked() {
                            self.calendar_view = view;
                            self.calendar_morning = true;
                        }
                    }
                    ui.add_space(8.0);
                    let step = |day: chrono::NaiveDate, view: calview::View, back: bool| match (view, back) {
                        (calview::View::Month, false) => day + chrono::Months::new(1),
                        (calview::View::Month, true) => day - chrono::Months::new(1),
                        (calview::View::Week, b) => day + chrono::Duration::days(if b { -7 } else { 7 }),
                        (calview::View::Agenda, b) => day + chrono::Duration::days(if b { -30 } else { 30 }),
                    };
                    if icons::button(ui, t, Icon::ChevronRight, "Next (→)", true, false).clicked() {
                        self.calendar_day = step(self.calendar_day, self.calendar_view, false);
                    }
                    if icons::button(ui, t, Icon::ChevronLeft, "Previous (←)", true, false).clicked() {
                        self.calendar_day = step(self.calendar_day, self.calendar_view, true);
                    }
                    if !week.is_empty() {
                        ui.label(RichText::new(week).size(13.0).color(t.text_dim));
                    }
                });
            });
            ui.add_space(8.0);
            view_action = match self.calendar_view {
                calview::View::Week => calview::week(ui, t, from, &items, today, std::mem::take(&mut self.calendar_morning)),
                calview::View::Month => calview::month(ui, t, from, self.calendar_day.month(), &items, today),
                calview::View::Agenda => calview::agenda(ui, t, from, days, &items, today),
            };
        });
        match view_action {
            Some(calview::ViewAction::Open(at)) => self.calendar_open = items.get(at).cloned(),
            Some(calview::ViewAction::Day(day)) => {
                (self.calendar_day, self.calendar_view, self.calendar_morning) = (day, calview::View::Week, true);
            }
            Some(calview::ViewAction::New(day, minutes)) => self.new_event(day, Some(minutes)),
            None => {}
        }
        if new_event {
            self.new_event(self.calendar_day, None);
        }
        if let Some(a) = action {
            self.calendar_action(a);
        }
        // The month on the left follows the day shown when another range is chosen.
        if self.calendar_day != day_before {
            self.calendar_month = self.calendar_day.with_day(1).unwrap_or(self.calendar_month);
        }
        if let Some(url) = url {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
    }

    fn book_action(&mut self, action: books::BookAction) {
        use books::BookAction as A;
        match action {
            A::Open => {
                // The login is most often the address of a mail account.
                let user = self.accounts.first().map(|v| &v.account).map(|a| if a.email.is_empty() { a.user.clone() } else { a.email.clone() }).unwrap_or_default();
                self.add_book = Some(books::AddBook { user, focus_first: true, ..Default::default() });
            }
            A::Find => self.find_books(),
            A::Show(url) => self.contacts_shown = url,
            A::Fold(key) => {
                if !self.settings.folded.remove(&key) {
                    self.settings.folded.insert(key);
                }
                self.save_settings();
            }
            A::Add(chosen) => {
                let Some(form) = self.add_book.take() else { return };
                for found in chosen {
                    // The first writable one becomes the default, if none is yet.
                    let default = found.writable && !self.settings.address_books.iter().any(|b| b.default);
                    let book = beavermail_core::AddressBook { name: found.name, url: found.url, user: form.user.trim().to_owned(), default, read_only: !found.writable };
                    if let Err(e) = store::set_book_password(&book, &form.password) {
                        self.error = Some(e);
                        return;
                    }
                    self.settings.address_books.push(book);
                }
                self.save_settings();
                self.sync_contacts(None);
            }
            A::MakeDefault(url) => {
                for book in &mut self.settings.address_books {
                    book.default = book.url == url;
                }
                self.save_settings();
            }
            A::AskRemove(url) => self.confirm_book_remove = Some(url),
            A::SyncNow => {
                self.contacts_synced = Some(std::time::Instant::now());
                self.sync_contacts(None);
            }
        }
    }

    /// No longer syncs the address book at `url`: its cards leave this computer, nothing changes on the server.
    fn remove_book(&mut self, url: &str) {
        let Some(at) = self.settings.address_books.iter().position(|b| b.url == url) else { return };
        let book = self.settings.address_books.remove(at);
        if let Err(e) = self.archive.forget_book(&book.url) {
            self.error = Some(format!("Could not forget the address book's contacts: {e}"));
        }
        // The password is shared by the books of one server and login: it goes with the last of them.
        if !self.settings.address_books.iter().any(|b| b.user == book.user && books::host(&b.url) == books::host(&book.url)) {
            let _ = store::forget_book_password(&book);
        }
        if book.default
            && let Some(next) = self.settings.address_books.iter_mut().find(|b| !b.read_only)
        {
            next.default = true;
        }
        self.save_settings();
        self.address_book = None;
        self.contact_pane = None;
        if self.contacts_shown.as_deref() == Some(url) {
            self.contacts_shown = None;
        }
    }

    fn book_remove_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(url) = self.confirm_book_remove.clone() else { return };
        let name = self.settings.address_books.iter().find(|b| b.url == url).map(|b| if b.name.is_empty() { books::host(&b.url).to_owned() } else { b.name.clone() }).unwrap_or_default();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_book_remove")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.85);
            ui.heading(format!("Stop syncing {name}?"));
            ui.label(RichText::new("Its contacts leave BeaverMail. Nothing changes on the server.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Stop syncing").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.remove_book(&url);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_book_remove = None;
        }
    }

    /// The address's own OpenPGP key.
    fn own_key(&mut self, address: &str) -> Option<Vec<u8>> {
        self.key_file(beavermail_crypto::key_path(address))
    }

    /// The address's own S/MIME certificate with its key.
    fn own_certificate(&mut self, address: &str) -> Option<Vec<u8>> {
        self.key_file(beavermail_crypto::certificate_path(address))
    }

    /// Keeps the address's own OpenPGP key, or (`certificate`) its S/MIME certificate, in the vault. A
    /// key's public half is also the key its own messages are checked with.
    fn keep_key(&mut self, address: &str, certificate: bool, data: Vec<u8>) {
        let address = address.trim().to_lowercase();
        let path = if certificate { beavermail_crypto::certificate_path(&address) } else { beavermail_crypto::key_path(&address) };
        if let Err(e) = self.vault.write(&path, &data) {
            self.error = Some(format!("Could not keep the key: {e}"));
            return;
        }
        if let Some(public) = beavermail_crypto::public_key(&data).filter(|_| !certificate) {
            let now = chrono::Local::now().timestamp();
            let _ = self.archive.learn_peer_key(&address, &public, now);
        }
        self.key_files.insert(path, Some(data));
        self.encryption_checked = None;
        self.key_dialogs.unlock = None;
        let told = if certificate { "Your certificate is imported" } else { "Your key is ready" };
        self.notice = Some((told.to_owned(), std::time::Instant::now()));
    }

    fn key_action(&mut self, action: encryption::KeyAction) {
        use encryption::KeyAction as A;
        match action {
            A::TurnOn(address) => {
                let name = self.accounts.iter().find(|v| v.account.address().eq_ignore_ascii_case(&address)).and_then(|v| v.account.sender_name().map(str::to_owned)).unwrap_or_default();
                match beavermail_crypto::generate(&name, &address) {
                    Ok(secret) => {
                        self.keep_key(&address, false, secret);
                        // Right away, while it is on the user's mind: a key only here is lost with the computer.
                        self.key_dialogs.backup = Some(encryption::Backup::new(address));
                    }
                    Err(e) => self.error = Some(format!("Could not make a key: {e}")),
                }
            }
            A::Import(address) => self.spawn(async move {
                let file = match rfd::AsyncFileDialog::new().set_title("Import a key or certificate").add_filter("OpenPGP key or S/MIME certificate", &["asc", "key", "gpg", "pgp", "p12", "pfx"]).pick_file().await {
                    Some(file) => Some((file.file_name(), file.read().await)),
                    None => None,
                };
                Event::KeyFile(address, file)
            }),
            A::Unlock(address, password) => {
                let Some(unlock) = self.key_dialogs.unlock.as_mut() else { return };
                let certificate = unlock.certificate;
                match import_file(&unlock.file, certificate, Some(&password)) {
                    Ok(data) => self.keep_key(&address, certificate, data),
                    Err(e) => unlock.error = Some(import_error(&e).to_owned()),
                }
            }
            A::AskBackup(address) => self.key_dialogs.backup = Some(encryption::Backup::new(address)),
            A::SaveBackup(address, code) => {
                let Some(secret) = self.own_key(&address) else { return };
                let backup = match beavermail_crypto::export(&secret, &code) {
                    Ok(backup) => backup,
                    Err(e) => return self.error = Some(format!("Could not back up the key: {e}")),
                };
                let name = format!("{}-key-backup.asc", address.replace('@', "-at-"));
                self.spawn(async move {
                    let Some(target) = rfd::AsyncFileDialog::new().set_title("Save the key backup").set_file_name(&name).save_file().await else { return Event::Saved(Ok(None)) };
                    let path = target.path().to_path_buf();
                    match std::fs::write(&path, backup) {
                        Ok(()) => Event::Saved(Ok(Some(format!("Key backup saved to {}", path.display())))),
                        Err(e) => Event::Saved(Err(format!("{}: {e}", path.display()))),
                    }
                });
            }
            A::AskRemove(address, certificate) => self.key_dialogs.remove = Some((address, certificate)),
            A::Remove(address, certificate) => {
                let path = if certificate { beavermail_crypto::certificate_path(&address) } else { beavermail_crypto::key_path(&address) };
                match self.vault.remove(&path) {
                    Ok(()) => self.notice = Some((if certificate { "The certificate is removed from this computer" } else { "The key is removed from this computer" }.to_owned(), std::time::Instant::now())),
                    Err(e) => self.error = Some(format!("Could not remove it: {e}")),
                }
                self.key_files.remove(&path);
                self.encryption_checked = None;
            }
        }
    }

    /// Opens the card of `address`; `name` is how the message showing it names them, for someone the
    /// archive does not know yet.
    fn open_card(&mut self, address: &str, name: &str) {
        self.card = self.load_card(address, name);
    }

    /// Someone's card as the archive knows them, with their latest conversations.
    fn load_card(&self, address: &str, name: &str) -> Option<card::Card> {
        // Someone the address book has opens as its card, with what it keeps of them.
        match self.archive.card_with_email(address).ok().flatten() {
            Some(stored) => self.load_book_card(stored.id),
            None => self.load_contact(address, name),
        }
    }

    /// A card of a synced address book: its name, notes, addresses and phones, with the conversations
    /// with its first address.
    fn load_book_card(&self, id: i64) -> Option<card::Card> {
        let stored = self.archive.card(id).ok().flatten()?;
        let first = stored.emails.first().cloned().unwrap_or_default();
        let mut card = self.load_contact(&first, &stored.name).unwrap_or_else(|| card::Card::new(beavermail_store::ContactCard::default(), Vec::new()));
        (card.data.name, card.data.notes, card.data.saved) = (stored.name.clone(), stored.note.clone(), true);
        (card.name, card.notes, card.book_card) = (stored.name.clone(), stored.note.clone(), Some(id));
        let books = &self.settings.address_books;
        card.details = Some(beavermail_carddav::vcard::details(&stored.vcard));
        card.read_only = books.iter().any(|b| b.url == stored.book && b.read_only);
        card.book = books.iter().find(|b| b.url == stored.book).map(|b| books::label(b, books)).unwrap_or_default();
        // The other address books with one of their addresses: the same person twice, kept apart.
        let others = self.archive.address_book().unwrap_or_default().into_iter().filter(|c| c.book != stored.book && c.emails.iter().any(|e| stored.emails.contains(e)));
        card.also_in = others.filter_map(|c| books.iter().find(|b| b.url == c.book).map(|b| books::label(b, books))).collect();
        card.also_in.dedup();
        card.can_add = false;
        Some(card)
    }

    /// Someone by address, as their saved contact or as mail knows them.
    fn load_contact(&self, address: &str, name: &str) -> Option<card::Card> {
        let address = address.trim().to_lowercase();
        if address.is_empty() {
            return None;
        }
        let data = self.archive.contact_card(&address).ok().flatten().unwrap_or_else(|| beavermail_store::ContactCard {
            name: if name.contains('@') { String::new() } else { name.trim().to_owned() },
            address: address.clone(),
            ..Default::default()
        });
        let keys: Vec<(String, u64)> = self.accounts.iter().map(|v| (v.account.key(), v.serial)).collect();
        let recent = self.archive.messages_with(&address, 12).unwrap_or_default().into_iter().filter_map(|(key, hit)| Some((keys.iter().find(|(k, _)| *k == key)?.1, hit))).collect();
        let mut card = card::Card::new(data, recent);
        card.can_add = self.default_book().is_some();
        Some(card)
    }

    fn card_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(card) = self.card.as_mut() else { return };
        if let Some(action) = card::show(ctx, t, card) {
            self.card_action(action, false);
        }
    }

    /// Does what was asked on a card: the dialog's, or (`inline`) the one in the Contacts section.
    fn card_action(&mut self, action: card::CardAction, inline: bool) {
        let Some(card) = (if inline { self.contact_pane.as_ref() } else { self.card.as_ref() }) else { return };
        let (address, name, notes, book_card, editing) = (card.data.address.clone(), card.name.clone(), card.notes.clone(), card.book_card, card.editing.clone());
        let reload = |app: &mut Self| {
            let fresh = match book_card {
                Some(id) => app.load_book_card(id),
                None => app.load_card(&address, &name),
            };
            *(if inline { &mut app.contact_pane } else { &mut app.card }) = fresh;
        };
        match action {
            // A card of the address book: changed there, and sent at once.
            card::CardAction::Save if book_card.is_some() => {
                let (Some(stored), Some(editing)) = (book_card.and_then(|id| self.archive.card(id).ok().flatten()), editing) else { return };
                let text = beavermail_carddav::vcard::apply(&stored.vcard, &editing);
                if let Err(e) = self.archive.edit_card(stored.id, &text, &beavermail_carddav::fields(&text)) {
                    self.error = Some(format!("Could not change the contact: {e}"));
                    return;
                }
                self.address_book = None;
                self.sync_book(&stored.book);
                reload(self);
            }
            card::CardAction::Remove if book_card.is_some() => self.confirm_card_delete = book_card,
            card::CardAction::AddToBook => {
                if self.add_to_book(&address, &name, &notes) {
                    reload(self);
                }
            }
            // Someone not in an address book: kept on this computer only. Only "Add to address book"
            // sends someone to the server, so no one gets there by accident.
            card::CardAction::Save => {
                if let Err(e) = self.archive.save_contact(&address, &name, &notes) {
                    self.error = Some(format!("Could not save the contact: {e}"));
                    return;
                }
                self.address_book = None;
                self.notice = Some((format!("{} is in your contacts", if name.trim().is_empty() { &address } else { name.trim() }), std::time::Instant::now()));
                reload(self);
            }
            card::CardAction::Remove => {
                if let Err(e) = self.archive.unsave_contact(&address) {
                    self.error = Some(format!("Could not remove the contact: {e}"));
                    return;
                }
                self.address_book = None;
                reload(self);
            }
            card::CardAction::Write => {
                if !inline {
                    self.card = None;
                }
                let to = beavermail_core::address_text(name.trim(), &address);
                self.new_message_to(&format!("{to}, "));
            }
            card::CardAction::Open(i) => {
                let Some((serial, hit)) = card.recent.get(i).cloned() else { return };
                self.card = None;
                self.show_settings = false;
                self.section = Section::Mail;
                self.open_hit(serial, &hit, false);
            }
            card::CardAction::Close => self.card = None,
            card::CardAction::PickPhoto => self.spawn(async move {
                let Some(file) = rfd::AsyncFileDialog::new().set_title("Choose a photo").add_filter("Picture", &["jpg", "jpeg", "png"]).pick_file().await else { return Event::ContactPhoto(None) };
                let bytes = file.read().await;
                Event::ContactPhoto(Some(tokio::task::spawn_blocking(move || card::shrink(&bytes)).await.unwrap_or_else(|e| Err(e.to_string()))))
            }),
        }
    }

    /// Puts someone in the default address book, sent at once; false when it could not.
    fn add_to_book(&mut self, address: &str, name: &str, notes: &str) -> bool {
        let Some(url) = self.default_book() else { return false };
        let uid = beavermail_carddav::new_uid();
        let text = beavermail_carddav::vcard::new_card(&uid, name, &[address.to_owned()], notes);
        if let Err(e) = self.archive.add_card(&url, &beavermail_carddav::new_href(&url, &uid), &text, &beavermail_carddav::fields(&text)) {
            self.error = Some(format!("Could not add the contact to the address book: {e}"));
            return false;
        }
        self.address_book = None;
        self.sync_book(&url);
        self.notice = Some((format!("{} is in your address book", if name.trim().is_empty() { address } else { name.trim() }), std::time::Instant::now()));
        true
    }

    /// The address book contacts added here go to: the one shown in Contacts, else the default one;
    /// never a read-only one.
    fn default_book(&self) -> Option<String> {
        let mut writable = self.settings.address_books.iter().filter(|b| !b.read_only);
        let first = writable.clone().next();
        let shown = writable.clone().find(|b| self.contacts_shown.as_deref() == Some(b.url.as_str()));
        shown.or(writable.find(|b| b.default)).or(first).map(|b| b.url.clone())
    }

    /// "New contact": an address, and a name if wished; their card opens to be saved.
    fn new_contact_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let book = self.default_book();
        let Some((address, name)) = self.new_contact.as_mut() else { return };
        let valid = beavermail_smtp::valid_address(address);
        let (mut add, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("new_contact")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading("New contact");
            let about = match &book {
                Some(url) => format!("Added to {}.", self.settings.address_books.iter().find(|b| b.url == *url).map(|b| if b.name.is_empty() { books::host(url).to_owned() } else { format!("{} on {}", b.name, books::host(url)) }).unwrap_or_default()),
                None => "Their card opens with what you type; save it there.".to_owned(),
            };
            ui.label(RichText::new(about).color(t.text_dim));
            ui.add_space(12.0);
            form_grid(ui, "new_contact_form", |ui| {
                ui.label("Email");
                let email = ui.add(field(address).hint_text("name@example.com"));
                // On opening only: once the user is in a field, it is theirs.
                if ui.memory(|m| m.focused().is_none()) {
                    email.request_focus();
                }
                ui.end_row();
                ui.label("Name");
                ui.add(field(name).hint_text("Optional"));
                ui.end_row();
            });
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                add = views::primary_button(ui, t, if book.is_some() { "Add contact" } else { "Open their card" }, valid).on_disabled_hover_text("Type an email address first").clicked() || (valid && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                cancel = ui.button("Cancel").clicked();
            });
        });
        if add && let Some((address, name)) = self.new_contact.take() {
            let address = address.trim().to_lowercase();
            // Someone the address book already has opens as they are: never a second card.
            if book.is_some() && self.archive.card_with_email(&address).ok().flatten().is_none() {
                self.add_to_book(&address, name.trim(), "");
            }
            self.contact_pane = self.load_card(&address, &name);
        } else if cancel || modal.should_close() {
            self.new_contact = None;
        }
    }

    /// New folder: its account (when there are several), the folder it goes inside, and its name.
    fn new_folder_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(mut form) = self.new_folder.take() else { return };
        let accounts: Vec<(u64, String)> = self.accounts.iter().filter(|v| v.source.is_some()).map(|v| (v.serial, v.account.address().to_owned())).collect();
        let folders: Vec<(String, Role)> = self
            .view(form.serial)
            .map(|v| views::folder_tree(&v.mailboxes).into_iter().map(|(i, _)| &v.mailboxes[i]).filter(|m| !v.in_trash_or_spam(&m.name)).map(|m| (m.name.clone(), m.role)).collect())
            .unwrap_or_default();
        let delimiter = self.view(form.serial).and_then(AccountView::delimiter);
        let shown = |name: &str, role: Role| views::folder_path(name, role, delimiter);
        let (mut create, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("new_folder")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading(if form.renaming.is_some() { "Rename folder" } else { "New folder" });
            let about = if form.renaming.is_some() { "Renamed on the server, with the folders inside it, so every device sees the new name." } else { "Made on the server, so every device that reads this account sees it." };
            ui.label(RichText::new(about).color(t.text_dim));
            ui.add_space(12.0);
            form_grid(ui, "new_folder_form", |ui| {
                if accounts.len() > 1 && form.renaming.is_none() {
                    ui.label("Account");
                    let current = accounts.iter().find(|(s, _)| *s == form.serial).map(|(_, a)| a.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("new_folder_account").width(FIELD_WIDTH).selected_text(current).show_ui(ui, |ui| {
                        for (serial, address) in &accounts {
                            if ui.selectable_label(form.serial == *serial, address).clicked() && form.serial != *serial {
                                (form.serial, form.parent) = (*serial, None);
                            }
                        }
                    });
                    ui.end_row();
                }
                if form.renaming.is_none() {
                    ui.label("Inside");
                    let inside = form.parent.as_deref().map_or("Top level".to_owned(), |p| shown(p, folders.iter().find(|(n, _)| n == p).map_or(Role::Other, |(_, r)| *r)));
                    egui::ComboBox::from_id_salt("new_folder_parent").width(FIELD_WIDTH).selected_text(inside).show_ui(ui, |ui| {
                        if ui.selectable_label(form.parent.is_none(), "Top level").clicked() {
                            form.parent = None;
                        }
                        for (name, role) in &folders {
                            if ui.selectable_label(form.parent.as_deref() == Some(name.as_str()), shown(name, *role)).clicked() {
                                form.parent = Some(name.clone());
                            }
                        }
                    });
                    ui.end_row();
                }
                ui.label("Name");
                let field = ui.add_enabled(!form.busy, field(&mut form.name).hint_text("Receipts"));
                // On opening only: once the user is in a field, it is theirs.
                if ui.memory(|m| m.focused().is_none()) {
                    field.request_focus();
                }
                if field.changed() {
                    form.error = None;
                }
                ui.end_row();
            });
            if let Some(error) = &form.error {
                ui.add_space(6.0);
                ui.add(egui::Label::new(RichText::new(error).color(t.danger)).wrap());
            }
            ui.add_space(12.0);
            let ready = !form.busy && !form.name.trim().is_empty();
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let label = match (form.busy, form.renaming.is_some()) {
                    (true, true) => "Renaming…",
                    (true, false) => "Creating…",
                    (false, true) => "Rename",
                    (false, false) => "Create folder",
                };
                create = views::primary_button(ui, t, label, ready).on_disabled_hover_text("Type a name first").clicked() || (ready && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                cancel = ui.button("Cancel").clicked();
                if form.busy {
                    ui.add(egui::Spinner::new().size(16.0));
                }
            });
        });
        if create && let Some(source) = self.view(form.serial).and_then(|v| v.source.clone()) {
            (form.busy, form.error) = (true, None);
            let (serial, parent, name) = (form.serial, form.parent.clone(), form.name.clone());
            match form.renaming.clone() {
                Some(from) => self.spawn(async move { Event::FolderRenamed(serial, from.clone(), source.rename_folder(&from, &name).await) }),
                None => self.spawn(async move { Event::FolderMade(serial, source.create_folder(parent.as_deref(), &name).await) }),
            }
        }
        if !(cancel || modal.should_close()) {
            self.new_folder = Some(form);
        }
    }

    /// Asks account `serial`'s server for its folders again, after one was made, renamed or deleted.
    fn refresh_folders(&mut self, serial: u64) {
        if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
            self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await, true) });
        }
    }

    /// Folder `from` of account `serial` is called `to` now, and those inside it with it: the names this
    /// app keeps follow at once (the folder shown, which folders are folded, moves waiting offline), so
    /// the list read again finds them.
    fn folder_renamed(&mut self, serial: u64, from: &str, to: &str) {
        let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) else { return };
        let (key, delimiter) = (view.account.key().to_owned(), view.delimiter());
        let renamed = |name: &str| match name.strip_prefix(from) {
            Some("") => Some(to.to_owned()),
            Some(rest) if views::is_inside(name, from, delimiter) => Some(format!("{to}{rest}")),
            _ => None,
        };
        for mailbox in &mut view.mailboxes {
            if let Some(new) = renamed(&mailbox.name) {
                mailbox.name = new;
            }
        }
        let prefix = folder_open_key(&key, "");
        self.settings.folded = std::mem::take(&mut self.settings.folded)
            .into_iter()
            .map(|k| match k.strip_prefix(&prefix).and_then(renamed) {
                Some(new) => folder_open_key(&key, &new),
                None => k,
            })
            .collect();
        self.save_settings();
        for change in self.offline_changes.iter_mut().filter(|c| c.0 == serial) {
            if let Action::Move(folder) = &mut change.3
                && let Some(new) = renamed(folder)
            {
                *folder = new;
            }
        }
        self.save_offline();
        self.refresh_folders(serial);
    }

    /// Confirms deleting a folder. It goes into the trash, with the folders inside it and their messages,
    /// to be brought back from there; one in the trash already, or one the server refused there, is deleted
    /// for good.
    fn folder_delete_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some((serial, name, refused)) = self.confirm_folder_delete.clone() else { return };
        let Some(view) = self.view(serial) else {
            self.confirm_folder_delete = None;
            return;
        };
        let delimiter = view.delimiter();
        let inside: Vec<&Mailbox> = view.mailboxes.iter().filter(|m| views::is_inside(&m.name, &name, delimiter)).collect();
        let messages: u32 = view.mailboxes.iter().filter(|m| m.name == name).chain(inside.iter().copied()).map(|m| m.total).sum();
        let title = views::folder_path(&name, Role::Other, delimiter);
        let in_trash = view.mailboxes.iter().any(|m| m.role == Role::Trash && views::is_inside(&name, &m.name, delimiter));
        let for_good = in_trash || refused.is_some();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_folder_delete")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(if for_good { format!("Delete {title} for good?") } else { format!("Move {title} to Trash?") });
            let folders = match inside.len() {
                0 => String::new(),
                n => format!(", with the {n} folder{} inside it,", if n == 1 { "" } else { "s" }),
            };
            let count = format!("{} message{}", views::thousands(messages), if messages == 1 { "" } else { "s" });
            let what = match &refused {
                Some(why) => format!("The server did not take it into the trash ({why}). The folder{folders} and its {count} can only be deleted from the server, and cannot be recovered."),
                None if for_good => format!("The folder{folders} and its {count} will be deleted from the server and cannot be recovered."),
                None => format!("The folder{folders} and its {count} go into Trash, where they can be brought back from."),
            };
            ui.add(egui::Label::new(RichText::new(what).color(t.text_dim)).wrap());
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = if for_good {
                    ui.add(egui::Button::new(RichText::new("Delete for good").color(t.on_accent)).fill(t.danger)).clicked()
                } else {
                    views::primary_button(ui, t, "Move to Trash", true).clicked()
                };
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm && let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
            match for_good {
                true => self.spawn(async move { Event::FolderDeleted(serial, name.clone(), source.delete_folder(&name).await) }),
                false => self.spawn(async move { Event::FolderTrashed(serial, name.clone(), source.trash_folder(&name).await) }),
            }
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_folder_delete = None;
        }
    }

    /// Syncs the address book at `url` now, to send a change made here.
    fn sync_book(&mut self, url: &str) {
        self.sync_contacts(Some(url));
    }

    /// Confirms deleting an address-book card: it goes from the server, so from every device that syncs it.
    fn card_delete_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(id) = self.confirm_card_delete else { return };
        let Some(stored) = self.archive.card(id).ok().flatten() else {
            self.confirm_card_delete = None;
            return;
        };
        let who = if stored.name.is_empty() { stored.emails.first().or(stored.phones.first()).cloned().unwrap_or_default() } else { stored.name.clone() };
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_card_delete")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.85);
            ui.heading(format!("Delete {who} everywhere?"));
            ui.label(RichText::new("They are deleted from your address book on the server, and from every device that syncs it.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Delete everywhere").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            match self.archive.delete_card(id) {
                Ok(()) => {
                    // Not left behind as a contact of this computer either.
                    for email in &stored.emails {
                        let _ = self.archive.unsave_contact(email);
                    }
                    if self.contact_pane.as_ref().is_some_and(|c| c.book_card == Some(id)) {
                        self.contact_pane = None;
                    }
                    if self.card.as_ref().is_some_and(|c| c.book_card == Some(id)) {
                        self.card = None;
                    }
                    self.address_book = None;
                    self.sync_book(&stored.book);
                }
                Err(e) => self.error = Some(format!("Could not delete the contact: {e}")),
            }
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_card_delete = None;
        }
    }

    /// Confirms removing an account: what was downloaded and the saved password go with it.
    fn remove_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(serial) = self.confirm_remove else { return };
        let Some(name) = self.view(serial).map(|v| v.account.address().to_owned()) else {
            self.confirm_remove = None;
            return;
        };
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_remove")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(format!("Remove {name}?"));
            ui.label(RichText::new("The mail downloaded to this computer and the saved password are deleted. Nothing changes on the server, so the account can be added again.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = ui.add(egui::Button::new(RichText::new("Remove account").color(t.on_accent)).fill(t.danger)).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.remove_account(serial);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_remove = None;
        }
    }

    /// Role of the folder being shown.
    fn role(&self) -> Role {
        if self.unified {
            return self.unified_role;
        }
        let (serial, index) = self.selected.unzip();
        serial.zip(index).and_then(|(s, i)| self.view(s)?.mailboxes.get(i)).map_or(Role::Other, |m| m.role)
    }

    /// Runs a user action; trashing from the trash asks first, because that erases for good.
    fn request(&mut self, id: MessageId, action: Action) {
        // A new command: what waited for Undo goes to the server now.
        self.send_pending();
        let whole = self.conversation(id);
        if matches!(action, Action::Reply | Action::ReplyAll | Action::Forward) {
            self.answer(id, action);
        } else if whole.len() > 1 {
            // Triage goes on with the conversation that takes this one's row, as for a single message.
            let row = self.listed().iter().position(|i| self.conversations.same(*i, id));
            let open = self.message.is_some_and(|m| whole.contains(&m));
            self.request_all(whole, action.clone());
            if action.removes() && open && self.confirm_erase.is_empty()
                && let Some(next) = row.and_then(|r| self.listed().get(r).copied())
            {
                self.move_to(next);
            }
        } else if matches!(action, Action::Trash) && self.role_of(id) == Role::Trash {
            self.confirm_erase = vec![id];
        } else {
            self.act(id, action);
        }
    }

    /// The messages a command applies to: those picked in the list, or else the one open.
    fn targets(&self) -> Vec<MessageId> {
        if self.picked.len() >= 2 { self.picked.clone() } else { self.message.into_iter().collect() }
    }

    /// Moves `ids` to folder `index` of account `serial`. To the archive, the trash or Spam it is the
    /// command of that name, so Spam learns from it too.
    fn move_to_folder(&mut self, ids: Vec<MessageId>, serial: u64, index: usize) {
        let Some(mailbox) = self.view(serial).and_then(|v| v.mailboxes.get(index)) else { return };
        let from_spam = ids.first().is_some_and(|id| self.role_of(*id) == Role::Junk);
        let action = match mailbox.role {
            Role::Archive => Action::Archive,
            Role::Trash => Action::Trash,
            Role::Junk => Action::Spam,
            Role::Inbox if from_spam => Action::NotSpam,
            _ => Action::Move(mailbox.name.clone()),
        };
        self.request_all(ids, action);
    }

    /// Runs `action` on several messages at once, as [`Self::request`] does on one. Spam and Not spam
    /// mark the senders and keep the messages as examples, as one at a time.
    fn request_all(&mut self, ids: Vec<MessageId>, action: Action) {
        self.send_pending();
        let mut ids: Vec<MessageId> = ids.into_iter().flat_map(|id| self.conversation(id)).collect();
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(*id));
        let Some(&first) = ids.first() else { return };
        if ids.len() == 1 && !matches!(action, Action::Spam | Action::NotSpam) {
            return self.request(first, action);
        }
        if matches!(action, Action::Trash) && self.role_of(first) == Role::Trash {
            self.confirm_erase = ids;
            return;
        }
        // The reader is left empty rather than walking through messages about to go.
        if action.removes() && self.message.is_some_and(|id| ids.contains(&id)) {
            self.message = None;
            self.body = None;
            self.in_tab = false;
        }
        match action {
            Action::Spam => self.move_to_spam(&ids),
            Action::NotSpam => ids.iter().for_each(|id| self.not_spam(*id)),
            _ => ids.iter().for_each(|id| self.act(*id, action.clone())),
        }
        // Marking read keeps the selection, as the messages are still there.
        if action.removes() {
            self.picked.clear();
        }
    }

    /// Ctrl+click: adds message `id` to the picked ones, or takes it out. The open message is the
    /// first picked, as a list shows it selected.
    fn toggle_pick(&mut self, id: MessageId) {
        if self.picked.is_empty()
            && let Some(open) = self.listed_open(&self.listed()).filter(|open| *open != id)
        {
            self.picked.push(open);
        }
        match self.picked.iter().position(|p| *p == id) {
            Some(at) => {
                self.picked.remove(at);
            }
            None => self.picked.push(id),
        }
        if let [only] = self.picked[..] {
            self.move_to(only);
        }
    }

    /// The row of `ids` showing the open message: in conversations, the row of its conversation.
    fn listed_open(&self, ids: &[MessageId]) -> Option<MessageId> {
        let open = self.message?;
        ids.iter().copied().find(|i| *i == open).or_else(|| ids.iter().copied().find(|i| self.threaded() && self.conversations.same(*i, open)))
    }

    /// Shift+click: picks every message from the first picked (or the open one) to `id`, in list order.
    fn pick_range(&mut self, id: MessageId) {
        let ids = self.listed();
        let Some(anchor) = self.picked.first().copied().or_else(|| self.listed_open(&ids)) else { return self.move_to(id) };
        if let Some(range) = range(&ids, anchor, id) {
            self.picked = range;
        }
    }

    /// Opens a reply or a forward of the open message.
    /// Opens a reply or a forward of message `id` with its text: the open message's, else read in the
    /// background (from this computer when stored), without opening the message.
    fn answer(&mut self, id: MessageId, action: Action) {
        if self.message == Some(id)
            && let (Some(tab), Some(open)) = (self.open_tab(), &self.body)
        {
            let (serial, body) = (tab.serial, open.data.clone());
            return self.answer_with(serial, id, action, &body);
        }
        let Some(serial) = self.serial_of(id) else { return };
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else {
            self.error = Some("Its account is not connected. Check its password in Settings.".into());
            return;
        };
        self.spawn(async move { Event::Answer(serial, id, action, source.body(id).await.map(Box::new)) });
    }

    /// Opens the compose window for a reply or a forward of message `id`, whose text is `body`.
    fn answer_with(&mut self, serial: u64, id: MessageId, action: Action, body: &MessageBody) {
        let Some(summary) = self.summary_of(id) else { return };
        let Some(view) = self.view(serial).filter(|v| v.sender.is_some()) else { return };
        let own = view.account.address();
        let key = view.account.key();
        let compose = match action {
            Action::Forward => compose::Compose::forward(serial, &key, summary, body, &body.attachments),
            _ => compose::Compose::reply(serial, own, summary, body, matches!(action, Action::ReplyAll)),
        };
        let mut compose = compose.with_signature(&view.account.signature);
        let carry = std::mem::take(&mut compose.carry);
        let source = view.source.clone();
        let draft = compose.id.clone();
        if self.compose.is_none() {
            self.start_compose(Some(compose));
            // The attachments of a forwarded message are copied in the background.
            if let Some(source) = source {
                for (index, slot) in carry {
                    let (vault, draft, source) = (self.vault.clone(), draft.clone(), source.clone());
                    self.spawn(async move {
                        let result = match source_attachment(&source, id, index).await {
                            Ok((_, bytes)) => vault.write(&slot, &bytes).map_err(|e| e.to_string()),
                            Err(e) => Err(e.to_string()),
                        };
                        Event::FileStored(draft, slot, result)
                    });
                }
            }
        }
    }

    /// Copies files into the open draft's folder in the vault, in the background.
    fn attach_files(&mut self, paths: Vec<std::path::PathBuf>) {
        let Some(compose) = self.compose.as_mut() else { return };
        let (key, draft) = (compose.key.clone(), compose.id.clone());
        for (source, slot) in compose.add_pending(&key, compose::usable(paths)) {
            let (vault, tx, ctx, draft) = (self.vault.clone(), self.tx.clone(), self.ctx.clone(), draft.clone());
            // Reading a big file must not hold up the window.
            self.runtime.spawn_blocking(move || {
                let result = std::fs::read(&source).map_err(|e| format!("{}: {e}", source.display())).and_then(|bytes| vault.write(&slot, &bytes).map_err(|e| e.to_string()));
                let _ = tx.send(Event::FileStored(draft, slot, result));
                ctx.request_repaint();
            });
        }
    }

    /// Saves the draft of a compose window (or removes it when nothing is written any more).
    fn save_draft(&mut self, compose: &mut compose::Compose) {
        if compose.is_empty() {
            self.delete_draft(compose);
        } else {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            match drafts::save(&self.vault, &compose.key, &compose.to_draft(now)) {
                Ok(()) => compose.mark_saved(),
                Err(e) => self.error = Some(format!("Could not save the draft: {e}")),
            }
        }
        self.refresh_drafts();
    }

    fn delete_draft(&mut self, compose: &compose::Compose) {
        if let Err(e) = self.forget_draft(compose.from, &compose.key, &compose.id, compose.may_be_on_server()) {
            self.error = Some(format!("Could not delete the draft: {e}"));
        }
    }

    /// Deletes draft `id` of account `serial` (whose key is `key`): on this computer, in the server's
    /// Drafts folder when a copy may be there, and from the lists at once, since the server's copy
    /// would otherwise show in Drafts until the next sync.
    fn forget_draft(&mut self, serial: u64, key: &str, id: &str, on_server: bool) -> Result<(), MailError> {
        let result = drafts::delete(&self.vault, key, id);
        if on_server && let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
            let gone = id.to_owned();
            self.spawn(async move { Event::DraftPushed(serial, gone.clone(), 0, source.delete_draft(&gone).await) });
        }
        let copy = beavermail_smtp::draft_message_id(id);
        self.messages.retain(|m| m.message_id != copy);
        self.refresh_drafts();
        self.count_outbox();
        result
    }

    /// Stores a copy of the draft in the account's Drafts folder, so other devices see it. The files are
    /// read from the vault and the message is built off the window's thread.
    fn push_draft(&mut self, compose: &mut compose::Compose) {
        let Some(view) = self.view(compose.from) else { return };
        let Some(source) = view.source.clone() else { return };
        let message = compose.outgoing(&view.account.sender(), self.settings.rich_outgoing);
        // A message meant to go encrypted is not left readable on the server as a draft: it is
        // encrypted to the user's own key.
        let address = view.account.address().to_owned();
        let to_self = if compose.encrypt { self.seal_to_self(&address) } else { None };
        let (files, vault, id, snapshot, serial) = (compose.file_list(), self.vault.clone(), compose.id.clone(), compose.snapshot(), compose.from);
        compose.pushing = true;
        self.spawn(async move {
            let built = tokio::task::spawn_blocking({
                let (id, mut message) = (id.clone(), message);
                move || -> Result<Vec<u8>, String> {
                    message.attachments = attachments::read_files(&vault, &files)?;
                    let raw = render_draft(&message, &id).map_err(|e| e.to_string())?;
                    match &to_self {
                        Some(seal) => beavermail_crypto::seal(&raw, seal),
                        None => Ok(raw),
                    }
                }
            })
            .await;
            let result = match built {
                Ok(Ok(raw)) => source.save_draft(&id, raw).await,
                Ok(Err(e)) => Err(MailError::Backend(e)),
                Err(e) => Err(MailError::Backend(e.to_string())),
            };
            Event::DraftPushed(serial, id, snapshot, result)
        });
    }

    /// Reads the drafts of the account whose Drafts folder is open.
    fn refresh_drafts(&mut self) {
        let key = self.selected.filter(|_| self.role() == Role::Drafts).and_then(|(s, _)| self.view(s)).map(|v| v.account.key());
        self.drafts = key.map(|key| drafts::load_all(&self.vault, &key)).unwrap_or_default();
    }

    fn open_draft(&mut self, id: &str) {
        let Some((serial, _)) = self.selected else { return };
        let Some(draft) = self.drafts.iter().find(|d| d.id == id).cloned() else { return };
        if self.compose.is_none() {
            self.start_compose(Some(compose::Compose::from_draft(serial, draft)));
        }
    }

    /// Up/Down move through the list, E archives, Delete trashes, U toggles unread.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.form.is_some() || !self.confirm_erase.is_empty() || self.confirm_spam || !self.confirm_empty.is_empty() || self.confirm_remove.is_some() || self.confirm_unsubscribe.is_some() || self.confirm_block.is_some() || self.compose.is_some() || self.show_settings || self.palette.is_some() || self.locked() {
            return;
        }
        // Ctrl+N and Ctrl+K work while the search box has the keyboard; the other keys do not.
        let typing = ctx.egui_wants_keyboard_input();
        if let Some(command) = Command::ALL.into_iter().find(|c| c.pressed(ctx, typing)) {
            if self.can(command).is_ok() {
                self.run(command);
            }
            return;
        }
        if typing || self.section != Section::Mail {
            return;
        }
        let key = |k| ctx.input(|i| i.key_pressed(k));
        // In a message tab the list is not on screen: no moving through it.
        let ids: Vec<MessageId> = if self.in_tab { Vec::new() } else { self.listed() };
        let pos = self.message.and_then(|id| ids.iter().position(|i| self.conversations.same(*i, id) && (self.threaded() || *i == id)));
        let reading = self.in_tab || self.pane == Some(Pane::Reader);
        if key(egui::Key::Escape) {
            self.picked.clear();
            if reading && !self.in_tab {
                self.pane = Some(Pane::List);
            }
        }
        let shift = ctx.input(|i| i.modifiers.shift);
        if reading {
            let scroll = match () {
                _ if key(egui::Key::ArrowDown) => Some(Scroll::Lines(1.0)),
                _ if key(egui::Key::ArrowUp) => Some(Scroll::Lines(-1.0)),
                _ if key(egui::Key::PageDown) || (key(egui::Key::Space) && !shift) => Some(Scroll::Pages(1.0)),
                _ if key(egui::Key::PageUp) || (key(egui::Key::Space) && shift) => Some(Scroll::Pages(-1.0)),
                _ if key(egui::Key::Home) => Some(Scroll::Edge(false)),
                _ if key(egui::Key::End) => Some(Scroll::Edge(true)),
                _ => None,
            };
            self.reader_scroll = scroll.or(self.reader_scroll);
        } else if !ids.is_empty() {
            let page = (self.list_view.1 / views::MESSAGE_ROW_H).floor().max(1.0) as isize;
            let last = ids.len() - 1;
            // With Shift the open message stays the anchor and the far end of the picked range moves.
            let anchor = if shift { self.picked.first().copied().or_else(|| self.listed_open(&ids)) } else { None };
            let at = |id: MessageId| ids.iter().position(|i| *i == id);
            let from = match anchor.and_then(at) {
                Some(a) => self.picked.iter().filter_map(|p| at(*p)).max_by_key(|p| p.abs_diff(a)).or(Some(a)),
                None => pos,
            };
            let next = match () {
                _ if key(egui::Key::ArrowDown) => Some(from.map_or(0, |p| p.saturating_add(1))),
                _ if key(egui::Key::ArrowUp) => Some(from.map_or(0, |p| p.saturating_sub(1))),
                _ if key(egui::Key::PageDown) => Some(from.map_or(0, |p| p.saturating_add_signed(page))),
                _ if key(egui::Key::PageUp) => Some(from.map_or(0, |p| p.saturating_add_signed(-page))),
                _ if key(egui::Key::Home) => Some(0),
                _ if key(egui::Key::End) => Some(last),
                _ => None,
            };
            if let Some(next) = next.map(|n| n.min(last)) {
                self.list_reveal = Some(next);
                if let Some(range) = anchor.and_then(|a| range(&ids, a, ids[next])) {
                    self.picked = if range.len() >= 2 { range } else { Vec::new() };
                } else if pos != Some(next) {
                    self.move_to(ids[next]);
                }
            }
        }
    }

    /// Whether `command` can run now, and if not, why (shown in the palette).
    fn can(&self, command: Command) -> Result<(), &'static str> {
        let many = self.picked.len() >= 2;
        let one = || match self.message {
            _ if many => Err("Several messages are picked"),
            None => Err("Open a message first"),
            Some(id) => Ok(id),
        };
        let some = || if many || self.message.is_some() { Ok(()) } else { Err("Open or pick a message first") };
        // A source tab shows text, not the message: what acts on a message is for its own tab.
        if self.showing_source() && !matches!(command, Command::Palette | Command::NewMessage | Command::Settings | Command::Mail | Command::Contacts | Command::WorkOffline | Command::WorkOnline | Command::CloseTab | Command::PreviousTab | Command::NextTab | Command::AllInboxes | Command::FindInSource) {
            return Err("Not in the source of a message");
        }
        // Out of Mail the list and the reader are not on screen: what acts on them waits.
        if self.section != Section::Mail && !matches!(command, Command::Palette | Command::NewMessage | Command::Settings | Command::Mail | Command::Contacts | Command::Calendar | Command::WorkOffline | Command::WorkOnline) {
            return Err("Go to Mail first");
        }
        match command {
            Command::Palette | Command::Search | Command::Settings | Command::Mail | Command::Contacts | Command::Calendar => Ok(()),
            Command::NewMessage if self.senders().is_empty() => Err("Add an account to send mail"),
            Command::NewMessage => Ok(()),
            // A message only selected, or still loading, is answered once its text is there.
            Command::Reply | Command::ReplyAll | Command::Forward => one().map(|_| ()),
            Command::Archive | Command::Trash | Command::ToggleRead | Command::Pin | Command::Star | Command::Label(_) => some(),
            Command::MoveTo => {
                some()?;
                let accounts: std::collections::HashSet<u64> = self.targets().iter().filter_map(|id| self.serial_of(*id)).collect();
                if accounts.len() > 1 { Err("The messages are in different accounts") } else { Ok(()) }
            }
            Command::NewFolder if self.work_offline => Err("Working offline"),
            Command::NewFolder => if self.accounts.iter().any(|v| v.source.is_some()) { Ok(()) } else { Err("Add an account first") },
            Command::Undo => if self.pending.is_some() { Ok(()) } else { Err("Nothing to undo") },
            Command::Spam => {
                some()?;
                let role = if many { self.picked_role() } else { self.message.map_or(Role::Other, |id| self.role_of(id)) };
                if role == Role::Inbox { Ok(()) } else { Err("Only for mail in the Inbox") }
            }
            Command::TrustSender => {
                let id = one()?;
                let key = self.summary_of(id).map(|m| screen::sender_key(&m.address)).unwrap_or_default();
                match self.role_of(id) {
                    Role::Junk => Err("Use Not spam for mail in Spam"),
                    Role::Sent | Role::Drafts => Err("Your own mail"),
                    _ if key.is_empty() => Err("The message has no sender address"),
                    _ if self.settings.mark_of(&key) == Some(true) => Err("Already trusted"),
                    _ => Ok(()),
                }
            }
            Command::BlockSender => {
                let id = one()?;
                match self.role_of(id) {
                    Role::Junk => Err("Already in Spam"),
                    Role::Sent | Role::Drafts => Err("Your own mail"),
                    _ if self.summary_of(id).is_none_or(|m| m.address.is_empty()) => Err("The message has no sender address"),
                    _ => Ok(()),
                }
            }
            Command::ViewSource => one().map(|_| ()),
            Command::FindInSource => if self.showing_source() { Ok(()) } else { Err("Only in the source of a message") },
            Command::OpenInTab => one().and(if self.in_tab { Err("Already in a tab") } else { Ok(()) }),
            Command::CloseTab => if self.in_tab { Ok(()) } else { Err("No message tab is shown") },
            Command::PreviousTab | Command::NextTab => if self.tabs.iter().any(|t| t.pinned) { Ok(()) } else { Err("No message is open in a tab") },
            Command::SelectAll => if self.in_tab || self.listed().is_empty() { Err("No list is shown") } else { Ok(()) },
            Command::Refresh if self.work_offline => Err("Working offline"),
            Command::Refresh => if self.shown().is_empty() { Err("No folder is shown") } else { Ok(()) },
            Command::SwitchPane if !self.settings.reading_pane => Err("The message is not shown beside the list"),
            Command::SwitchPane => if self.in_tab { Err("A message tab has no list") } else { Ok(()) },
            Command::AllInboxes => if self.accounts.len() >= 2 { Ok(()) } else { Err("Needs two accounts or more") },
            Command::WorkOffline => if self.work_offline { Err("Already working offline") } else { Ok(()) },
            Command::WorkOnline => if self.work_offline { Ok(()) } else { Err("Already online") },
        }
    }

    /// Runs `command`, from its key or the palette; [`Self::can`] said yes.
    fn run(&mut self, command: Command) {
        let targets = self.targets();
        let answer = |c| match c {
            Command::Reply => Action::Reply,
            Command::ReplyAll => Action::ReplyAll,
            _ => Action::Forward,
        };
        match command {
            Command::Palette => self.palette = Some(Palette::default()),
            Command::NewMessage => self.new_message(),
            Command::Reply | Command::ReplyAll | Command::Forward => {
                if let Some(id) = self.message {
                    self.request(self.latest_of(id), answer(command));
                }
            }
            Command::Archive => self.request_all(targets, Action::Archive),
            Command::Trash => self.request_all(targets, Action::Trash),
            Command::MoveTo => self.palette = Some(Palette { moving: targets, ..Palette::default() }),
            Command::NewFolder => {
                // In the account shown, at the top; the dialog can change both.
                let serial = self.shown().first().map(|(s, _)| *s).filter(|s| self.view(*s).is_some_and(|v| v.source.is_some()));
                if let Some(serial) = serial.or_else(|| self.accounts.iter().find(|v| v.source.is_some()).map(|v| v.serial)) {
                    self.new_folder = Some(NewFolder { serial, parent: None, name: String::new(), busy: false, error: None, renaming: None });
                }
            }
            Command::Undo => self.undo(),
            Command::ToggleRead => {
                let unread = targets.iter().any(|id| self.summary_of(*id).is_some_and(|m| m.unread));
                self.request_all(targets, Action::Unread(!unread));
            }
            Command::Pin => {
                // Pins them unless every one is pinned already.
                let pinned = targets.iter().all(|id| self.summary_of(*id).is_some_and(|m| m.pinned));
                self.request_all(targets, Action::Pin(!pinned));
            }
            Command::Star => {
                let starred = targets.iter().all(|id| self.summary_of(*id).is_some_and(|m| m.starred));
                self.request_all(targets, Action::Star(!starred));
            }
            // On unless every one has it already.
            Command::Label(n) => {
                let keyword = format!("$label{n}");
                let has = targets.iter().all(|id| self.summary_of(*id).is_some_and(|m| m.labels.contains(&keyword)));
                self.request_all(targets, Action::Label(n, !has));
            }
            Command::Spam if self.picked.len() >= 2 => self.request_all(targets, Action::Spam),
            Command::Spam => {
                if let Some(id) = self.message {
                    self.correct(id, Correction::Spam);
                }
            }
            Command::TrustSender => {
                if let Some(id) = self.message {
                    self.trust(id);
                }
            }
            Command::BlockSender => self.confirm_block = self.message,
            Command::ViewSource => {
                if let Some(id) = self.message {
                    self.view_source(id);
                }
            }
            Command::FindInSource => {
                if let Some(view) = &mut self.source_view {
                    view.find.get_or_insert_with(Find::default).focus = true;
                }
            }
            Command::OpenInTab => {
                if let Some(id) = self.message {
                    self.open_message(id, true);
                }
            }
            Command::CloseTab => {
                if let Some(at) = self.active().filter(|_| self.in_tab) {
                    self.close_tab(at);
                }
            }
            Command::PreviousTab => self.cycle_tabs(true),
            Command::NextTab => self.cycle_tabs(false),
            Command::SelectAll => self.picked = self.listed(),
            Command::Search => self.focus_search = true,
            Command::Refresh => self.refresh(),
            Command::SwitchPane => self.pane = Some(if self.pane == Some(Pane::Reader) || self.message.is_none() { Pane::List } else { Pane::Reader }),
            Command::AllInboxes => {
                self.show_suspicious = false;
                self.show_all_inboxes();
            }
            Command::Settings => {
                self.show_settings = true;
                self.security.mode = None;
            }
            Command::WorkOffline => self.set_work_offline(true),
            Command::WorkOnline => self.set_work_offline(false),
            Command::Mail => self.section = Section::Mail,
            Command::Contacts => self.section = Section::Contacts,
            Command::Calendar => {
                self.section = Section::Calendar;
                self.calendar_morning = true;
            }
        }
    }

    /// Adds the account, or applies an edit. The password goes to the credential store first and
    /// the settings file second, so a failure never leaves one without the other.
    /// Saves the form as a new or changed account and returns the serial it is shown under now.
    fn save_account(&mut self, form: &NewAccount) -> Result<u64, String> {
        let account = form.build()?;
        let editing = form.editing.and_then(|serial| self.accounts.iter().position(|v| v.serial == serial));
        let old = editing.map(|at| self.accounts[at].account.clone());
        if self.accounts.iter().enumerate().any(|(i, v)| Some(i) != editing && v.account.key() == account.key()) {
            return Err("this account is already configured".into());
        }
        let mut all: Vec<Account> = self.accounts.iter().map(|v| v.account.clone()).collect();
        if let Some(at) = editing
            && form.password.is_empty()
            && form.smtp_password.is_empty()
            && old.as_ref().is_some_and(|o| o.key() == account.key() && o.imap == account.imap && o.smtp == account.smtp && o.smtp_user == account.smtp_user)
        {
            // Name, address, signature or sync choices only: the password, the connection and the open folder stay as they are.
            all[at] = account.clone();
            store::save(&all)?;
            self.accounts[at].account = account;
            return Ok(self.accounts[at].serial);
        }
        let password = match (&old, form.password.is_empty()) {
            (Some(old), true) => store::password(old)?, // moving the stored password to the new key
            _ => form.password.clone(),
        };
        let smtp_password = match (&account.smtp_user, &old, form.smtp_password.is_empty()) {
            (None, ..) => None,
            (Some(_), _, false) => Some(form.smtp_password.clone()),
            (Some(_), Some(old), true) if old.smtp_user.is_some() => Some(store::smtp_password(old)?), // kept, or moved to the new key
            (Some(_), ..) => return Err("enter the password for the outgoing server".into()),
        };
        store::set_password(&account, &password)?;
        match &smtp_password {
            Some(smtp) => store::set_smtp_password(&account, smtp)?,
            None => store::forget_smtp_password(&account)?, // the same login as the incoming server again
        }
        match editing {
            Some(at) => all[at] = account.clone(),
            None => all.push(account.clone()),
        }
        let key_changed = old.as_ref().is_none_or(|o| o.key() != account.key());
        if let Err(e) = store::save(&all) {
            if key_changed {
                let _ = store::forget_password(&account); // do not leave a secret without settings
            }
            return Err(e);
        }
        let Some(at) = editing else {
            return Ok(self.add_view(account));
        };
        let old = self.accounts.remove(at);
        old.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        old.watch_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if key_changed {
            let _ = store::forget_password(&old.account);
            self.forget_archive(&old.account); // its UIDs mean nothing on another server
        }
        let serial = self.insert_view(account, Some(at));
        if self.selected.is_some_and(|(s, _)| s == old.serial) {
            self.selected = None;
            self.messages.clear();
            self.message = None;
            self.body = None;
        }
        if self.unified {
            self.show_all(self.unified_role); // its mail under the new connection
        }
        if let Some(compose) = &mut self.compose
            && compose.from == old.serial
        {
            compose.from = serial;
        }
        Ok(serial)
    }

    /// Deletes everything downloaded for an account: its index rows and its message files.
    fn forget_archive(&mut self, account: &Account) {
        let key = account.key();
        let result = self.archive.forget_account(&key).and_then(|()| self.vault.remove_dir(&beavermail_vault::account_path(&key)));
        if let Err(e) = result {
            self.error = Some(e.to_string());
        }
    }

    fn remove_account(&mut self, serial: u64) {
        let Some(position) = self.accounts.iter().position(|v| v.serial == serial) else { return };
        let view = self.accounts.remove(position);
        self.tabs.retain(|t| t.serial != serial);
        if self.open_tab().is_none() {
            self.message = None;
            self.body = None;
        }
        view.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        view.watch_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.forget_archive(&view.account);
        let all: Vec<Account> = self.accounts.iter().map(|v| v.account.clone()).collect();
        if let Err(e) = store::save(&all).and_then(|()| store::forget_password(&view.account)) {
            self.error = Some(e);
        }
        if self.selected.is_some_and(|(s, _)| s == serial) {
            self.selected = None;
            self.messages.clear();
            self.message = None;
            self.body = None;
        }
        if self.unified {
            self.show_all(self.unified_role); // without the account's mail
        }
    }

    fn apply(&mut self, event: Event) {
        if let Event::Done(serial, _, Some((id, _)), _) = &event
            && let Some(at) = self.in_flight.iter().position(|c| c.0 == *serial && c.2 == *id)
        {
            self.in_flight.remove(at);
        }
        match event {
            Event::Mailboxes(serial, result, fresh) => {
                let selected_name = self.selected.filter(|(s, _)| *s == serial).and_then(|(_, i)| self.view(serial)?.mailboxes.get(i)).map(|m| m.name.clone());
                let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) else { return };
                if !fresh && !view.mailboxes.is_empty() {
                    // The archive only has fresher counts of what is stored; the server's unread counts stay.
                    if let Ok(cached) = result {
                        for folder in &mut view.mailboxes {
                            if let Some(row) = cached.iter().find(|c| c.name == folder.name) {
                                folder.stored = row.stored;
                            }
                        }
                    }
                    return;
                }
                let mut download = false;
                let lost = fresh && matches!(result, Err(MailError::Offline(_)));
                let unread_before = view.mailboxes.iter().find(|m| m.role == Role::Inbox).map(|m| m.unread);
                match result {
                    Ok(mut mailboxes) => {
                        download = fresh;
                        // New mail since the last time the server was asked (not the first answer).
                        let unread_now = mailboxes.iter().find(|m| m.role == Role::Inbox).map(|m| m.unread);
                        if fresh && let (Some(before), Some(now)) = (unread_before, unread_now) && now > before && !view.mailboxes.is_empty() {
                            self.notice = Some((format!("{} new message{}", now - before, if now - before == 1 { "" } else { "s" }), std::time::Instant::now()));
                        }
                        // Same order for the archive's list and the server's, so a position means the same folder.
                        mailboxes.sort_by_key(|m| (views::folder_rank(m.role), m.name.clone()));
                        view.mailboxes = mailboxes;
                        view.error = None;
                        if fresh {
                            view.offline = None; // the server answered
                        }
                        let inbox = view.mailboxes.iter().position(|m| m.role == Role::Inbox).unwrap_or(0);
                        let joins = view.mailboxes.iter().position(|m| m.role == self.unified_role);
                        let moved = selected_name.and_then(|name| view.mailboxes.iter().position(|m| m.name == name));
                        if self.selected.is_some_and(|(s, _)| s == serial) {
                            self.selected = moved.map(|i| (serial, i));
                        } else if self.unified {
                            // An account whose folders arrive after All inboxes was opened joins it now.
                            if let Some(folder) = joins.filter(|_| !self.origin.values().any(|s| *s == serial)) {
                                self.load_folder(serial, folder);
                            }
                        } else if self.selected.is_none() && !view.mailboxes.is_empty() {
                            if self.accounts.len() >= 2 { self.show_all_inboxes() } else { self.select_mailbox(serial, inbox) }
                        }
                    }
                    Err(MailError::Offline(why)) => (view.offline, view.error) = (Some(why), None),
                    Err(e) => view.error = Some(e.to_string()),
                }
                if lost {
                    self.retry_later(serial);
                }
                if download {
                    if let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) {
                        view.retry_wait = RETRY_FIRST;
                    }
                    self.start_download(serial);
                    self.start_watch(serial);
                    self.send_outbox(serial);
                }
            }
            Event::Retried(serial, result) => {
                if let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) {
                    view.retrying = false;
                }
                self.apply(Event::Mailboxes(serial, result, true));
            }
            Event::Found(seq, serial, result) => {
                if seq != self.search_seq {
                    return; // an answer to a query that has been changed since
                }
                if self.hits_seq != seq {
                    self.hits.clear();
                    self.hits_seq = seq;
                }
                match result {
                    Ok(found) => {
                        // As the list: the Inbox without what is held as suspicious, Suspicious only that.
                        let want = if self.show_suspicious { screen::Place::Spam } else { screen::Place::Inbox };
                        let holding = self.holding();
                        let fits = |h: &Hit| !holding || self.screening.get(&h.message.id).map_or(!self.show_suspicious, |s| s.place == want);
                        let found: Vec<Hit> = found.into_iter().filter(fits).collect();
                        self.hits.extend(found.into_iter().map(|h| (serial, h)));
                        self.hits.sort_by_key(|(_, h)| std::cmp::Reverse(h.message.timestamp));
                        self.hits.truncate(300);
                    }
                    Err(e) => self.error = Some(format!("Search failed: {e}")),
                }
            }
            Event::Changed(serial) => {
                // Bring the Inbox up to date and refresh the counts; the list reloads if the Inbox is open.
                if let Some(view) = self.view(serial)
                    && let Some(source) = view.source.clone()
                    && let Some((index, name)) = view.mailboxes.iter().enumerate().find(|(_, m)| m.role == Role::Inbox).map(|(i, m)| (i, m.name.clone()))
                {
                    let folders = source.clone();
                    self.spawn(async move { Event::Mailboxes(serial, folders.mailboxes().await, true) });
                    self.start_sync(serial, index, source, name);
                }
            }
            Event::Downloaded(serial, result) => {
                match result {
                    Ok(()) => {}
                    // The status bar already says the stored mail is locked.
                    Err(MailError::Auth(_)) if self.vault.is_locked() => {}
                    Err(MailError::Offline(why)) => self.went_offline(serial, why),
                    Err(e) => self.error = Some(format!("Downloading mail stopped: {e}")),
                }
                // Previews arrive with the files.
                if let Some((s, index)) = self.shown().into_iter().find(|(s, _)| *s == serial)
                    && let Some(view) = self.view(s)
                    && let (Some(source), Some(name)) = (view.source.clone(), view.mailboxes.get(index).map(|m| m.name.clone()))
                {
                    let counts = source.clone();
                    self.spawn(async move { Event::Messages(serial, index, source.messages(&name).await) });
                    self.spawn(async move { Event::Mailboxes(serial, counts.cached_mailboxes().await, false) });
                }
            }
            Event::Synced(serial, index, result) => {
                self.syncing = self.syncing.saturating_sub(1);
                if !self.shows(serial, index) {
                    return;
                }
                match result {
                    Ok(report) => {
                        if let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) {
                            view.offline = None; // the server answered
                        }
                        let Some(view) = self.view(serial) else { return };
                        let (Some(source), Some(name)) = (view.source.clone(), view.mailboxes.get(index).map(|m| m.name.clone())) else { return };
                        let (local, folder) = (source.clone(), name.clone());
                        self.spawn(async move { Event::Messages(serial, index, local.messages(&folder).await) });
                        let counts = source.clone();
                        self.spawn(async move { Event::Mailboxes(serial, counts.cached_mailboxes().await, false) });
                        if report.remaining > 0 {
                            self.start_sync(serial, index, source, name);
                        }
                    }
                    Err(MailError::Offline(why)) => self.went_offline(serial, why),
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            Event::Messages(serial, index, result) if self.shows(serial, index) => match result {
                Ok(messages) => {
                    // This account's part of the list is replaced; the others' stay (All inboxes).
                    let origin = &self.origin;
                    self.messages.retain(|m| origin.get(&m.id).is_some_and(|s| *s != serial));
                    self.origin.extend(messages.iter().map(|m| (m.id, serial)));
                    // Mail waiting for Undo is still in the folder on the server and in the archive.
                    let offline_moves = self.offline_changes.iter().filter(|c| c.3.removes()).map(|c| c.2);
                    let waiting: Vec<MessageId> = self.pending.iter().flat_map(|p| p.items.iter().map(|(_, _, _, m, _)| m.id)).chain(offline_moves).collect();
                    self.messages.extend(messages.into_iter().filter(|m| !waiting.contains(&m.id)));
                    if self.unified {
                        self.messages.sort_by_key(|m| std::cmp::Reverse(m.timestamp));
                    }
                    self.error = None;
                    self.screen_inbox(serial, index);
                }
                Err(e) => self.error = Some(e.to_string()),
            },
            Event::SentList(serial, index, result) if self.shows(serial, index) => {
                // Without it a conversation shows only the folder's side; not worth an error.
                let origin = &self.origin;
                self.sent.retain(|m| origin.get(&m.id).is_some_and(|s| *s != serial));
                let sent = result.unwrap_or_default();
                self.origin.extend(sent.iter().map(|m| (m.id, serial)));
                self.sent.extend(sent);
            }
            Event::Screened(serial, index, result) if self.shows(serial, index) => {
                self.checking = false;
                match result {
                    Ok(verdicts) => {
                        self.screening.extend(verdicts);
                        self.move_marked_spam();
                    }
                    Err(e) => {
                        // Without verdicts the Inbox shows everything rather than hiding mail.
                        self.screening.clear();
                        self.error = Some(format!("The spam checks could not run, so the Inbox shows all mail: {e}"));
                    }
                }
            }
            Event::Unfolded(id, result) => {
                if let Some(unfolded) = self.unfolded.iter_mut().find(|u| u.id == id) {
                    unfolded.body = Some(result.map(|p| OpenBody::new(*p)).map_err(|e| e.to_string()));
                }
            }
            Event::Answer(serial, id, action, result) => match result {
                Ok(body) => self.answer_with(serial, id, action, &body),
                Err(MailError::Offline(why)) => {
                    self.went_offline(serial, why);
                    self.error = Some("The message is not stored on this computer, and the server cannot be reached to answer it.".into());
                }
                Err(e) => self.error = Some(format!("Could not read the message to answer it: {e}")),
            },
            Event::Source(id, result) => {
                if let Some(view) = self.source_view.as_mut().filter(|v| v.id == id) {
                    view.size = result.as_ref().map_or(0, Vec::len);
                    view.lines = Some(result.map(|raw| String::from_utf8_lossy(&raw).lines().map(str::to_owned).collect()).map_err(|e| e.to_string()));
                }
            }
            Event::Body(serial, id, result) if self.message == Some(id) && self.open_tab().is_some_and(|t| t.serial == serial) =>
            {
                match result {
                    Ok(prepared) => self.body = Some(OpenBody::new(*prepared)),
                    Err(MailError::Offline(why)) => {
                        self.went_offline(serial, why);
                        self.body_failed = Some((id, "It is not stored on this computer, and the server cannot be reached. It opens once it can.".into()));
                    }
                    Err(e) => self.body_failed = Some((id, e.to_string())),
                }
            }
            // No connection: the change waits and is made when the server answers again.
            Event::FolderMade(serial, Ok(full)) => {
                if self.new_folder.as_ref().is_some_and(|f| f.serial == serial) {
                    self.new_folder = None;
                }
                let title = views::folder_title(&full, Role::Other, self.view(serial).and_then(AccountView::delimiter));
                self.notice = Some((format!("Folder {title} created"), std::time::Instant::now()));
                self.refresh_folders(serial);
            }
            Event::FolderRenamed(serial, from, Ok(to)) => {
                if self.new_folder.as_ref().is_some_and(|f| f.serial == serial) {
                    self.new_folder = None;
                }
                self.folder_renamed(serial, &from, &to);
                let title = views::folder_title(&to, Role::Other, self.view(serial).and_then(AccountView::delimiter));
                self.notice = Some((format!("Folder renamed to {title}"), std::time::Instant::now()));
            }
            Event::FolderTrashed(serial, name, Ok(to)) => {
                self.folder_renamed(serial, &name, &to);
                let title = views::folder_title(&name, Role::Other, self.view(serial).and_then(AccountView::delimiter));
                self.notice = Some((format!("Folder {title} moved to Trash"), std::time::Instant::now()));
            }
            Event::FolderTrashed(_, _, Err(MailError::Offline(why))) => self.error = Some(format!("The folder was not moved to Trash: {why}")),
            // The server keeps no folders in its trash: the user is asked again, about deleting it for good.
            Event::FolderTrashed(serial, name, Err(e)) => self.confirm_folder_delete = Some((serial, name, Some(e.to_string()))),
            Event::FolderRenamed(serial, _, Err(e)) | Event::FolderDeleted(serial, _, Err(e)) => match self.new_folder.as_mut().filter(|f| f.serial == serial) {
                Some(form) => (form.busy, form.error) = (false, Some(e.to_string())),
                None => self.error = Some(format!("The folder was not changed: {e}")),
            },
            Event::FolderDeleted(serial, name, Ok(())) => {
                let delimiter = self.view(serial).and_then(AccountView::delimiter);
                let shown = self.selected.filter(|(s, _)| *s == serial).and_then(|(_, i)| self.view(serial)?.mailboxes.get(i)).map(|m| m.name.clone());
                // The folder shown went: the Inbox takes its place.
                if shown.is_some_and(|open| open == name || views::is_inside(&open, &name, delimiter))
                    && let Some(inbox) = self.view(serial).and_then(|v| v.mailboxes.iter().position(|m| m.role == Role::Inbox))
                {
                    self.select_mailbox(serial, inbox);
                }
                // Gone from the sidebar at once, not when the server's list comes; the folder shown keeps
                // its place by name, as positions move.
                let shown = self.selected.filter(|(s, _)| *s == serial).and_then(|(_, i)| self.view(serial)?.mailboxes.get(i)).map(|m| m.name.clone());
                if let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) {
                    view.mailboxes.retain(|m| m.name != name && !views::is_inside(&m.name, &name, delimiter));
                    if let Some(open) = shown {
                        self.selected = view.mailboxes.iter().position(|m| m.name == open).map(|i| (serial, i));
                    }
                }
                self.notice = Some((format!("Folder {} deleted", views::folder_title(&name, Role::Other, delimiter)), std::time::Instant::now()));
                self.refresh_folders(serial);
            }
            Event::FolderMade(serial, Err(e)) => match self.new_folder.as_mut().filter(|f| f.serial == serial) {
                Some(form) => (form.busy, form.error) = (false, Some(e.to_string())),
                None => self.error = Some(format!("The folder was not created: {e}")),
            },
            Event::Done(serial, index, Some(change), Err(MailError::Offline(why))) => {
                self.went_offline(serial, why);
                self.offline_changes.push((serial, index, change.0, change.1));
                self.save_offline();
            }
            Event::Done(serial, index, _, Err(e)) => {
                self.error = Some(e.to_string());
                // Show what the server really has.
                if self.unified && self.shows(serial, index) {
                    self.show_all(self.unified_role);
                } else if self.selected == Some((serial, index)) {
                    self.select_mailbox(serial, index);
                }
            }
            // Emptying Spam or Trash: some servers move them to Trash instead, so the counts are asked again.
            Event::Done(serial, _, None, Ok(())) => {
                if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
                    self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await, true) });
                }
            }
            Event::Done(..) => {}
            Event::Vault(outcome) => self.apply_vault(outcome),
            Event::Attachment(result) => match result {
                Ok((name, bytes)) => match attachments::open(&self.open_dir, &name, &bytes) {
                    Ok(()) => self.notice = Some((format!("Opening {}", attachments::safe_name(&name)), std::time::Instant::now())),
                    Err(e) => self.error = Some(e),
                },
                Err(e) => self.error = Some(format!("Could not get the attachment: {e}")),
            },
            Event::DraftPushed(serial, id, snapshot, result) => {
                if let Some(compose) = self.compose.as_mut().filter(|c| c.id == id) {
                    compose.pushing = false;
                    if result.is_ok() && snapshot != 0 {
                        compose.server_saved = snapshot;
                    }
                }
                // A copy that was being stored while the draft was discarded would live on, hidden from nobody.
                if result.is_ok() && snapshot != 0 && self.compose.as_ref().is_none_or(|c| c.id != id) {
                    let key = self.view(serial).map(|v| v.account.key());
                    let source = self.view(serial).and_then(|v| v.source.clone());
                    if let (Some(key), Some(source)) = (key, source)
                        && !drafts::exists(&self.vault, &key, &id)
                    {
                        let gone = id.clone();
                        self.spawn(async move { Event::DraftPushed(serial, gone.clone(), 0, source.delete_draft(&gone).await) });
                    }
                }
                match result {
                    Ok(()) => {
                        self.draft_error = None;
                        // The account's Drafts folder changed: bring that into the list.
                        if let Some(view) = self.view(serial)
                            && let Some(source) = view.source.clone()
                            && let Some((index, name)) = view.mailboxes.iter().enumerate().find(|(_, m)| m.role == Role::Drafts).map(|(i, m)| (i, m.name.clone()))
                        {
                            self.start_sync(serial, index, source, name);
                        }
                    }
                    Err(e) => self.draft_error = Some(e.to_string()),
                }
            }
            Event::Picked(paths) => self.attach_files(paths),
            Event::FileStored(draft, slot, result) => {
                if let Some(compose) = self.compose.as_mut().filter(|c| c.id == draft) {
                    compose.file_done(&slot, result.is_ok());
                    if let Err(e) = result {
                        compose.error = Some(format!("Could not add the attachment: {e}"));
                    }
                }
            }
            Event::SignedIn(cancel, result) => self.signed_in(cancel, result),
            Event::Checked(editing, tested, imap, smtp) => {
                let form = if editing.is_none() { self.form.as_mut() } else { self.account_edit.as_mut().filter(|f| f.editing == editing) };
                if let Some(check) = form.and_then(|f| f.check.as_mut()).filter(|c| c.tested == *tested) {
                    check.answers = Some((imap, smtp));
                }
            }
            Event::KeyFile(address, Some((name, file))) => {
                let certificate = [".p12", ".pfx"].iter().any(|e| name.to_lowercase().ends_with(e));
                match import_file(&file, certificate, None) {
                    Ok(data) => self.keep_key(&address, certificate, data),
                    Err(beavermail_crypto::ImportError::NeedsPassword) => self.key_dialogs.unlock = Some(encryption::Unlock { address, file, certificate, password: String::new(), error: None }),
                    Err(e) => self.error = Some(import_error(&e).to_owned()),
                }
            }
            Event::KeyFile(_, None) => {}
            Event::ContactPhoto(None) => {}
            Event::ContactPhoto(Some(Ok(photo))) => {
                for card in [self.contact_pane.as_mut(), self.card.as_mut()].into_iter().flatten() {
                    if let Some(editing) = card.editing.as_mut() {
                        editing.photo = Some(photo.clone());
                    }
                }
            }
            Event::ContactPhoto(Some(Err(e))) => self.error = Some(e),
            Event::ContactsSynced(key, result) => {
                self.contacts_syncing.remove(&key);
                match result {
                    Ok(_) => self.address_book = None, // read again, with what came
                    // No connection now: the next sync tries again, with nothing to tell.
                    Err(MailError::Offline(_)) => {}
                    Err(e) => self.error = Some(format!("Contacts could not be synced: {e}")),
                }
            }
            Event::CalendarsSynced(url, result) => {
                self.calendars_syncing.remove(&url);
                match result {
                    Ok(_) => self.calendar_version += 1,
                    Err(MailError::Offline(_)) => {}
                    Err(e) => self.error = Some(format!("A calendar could not be synced: {e}")),
                }
            }
            Event::CalendarWritten(result) => {
                self.calendar_version += 1;
                match result {
                    Ok(()) => {}
                    Err(MailError::Offline(_)) => self.error = Some("The calendar's server cannot be reached: the event is not in it yet. Answer again once online.".into()),
                    Err(e) => self.error = Some(format!("The calendar could not be changed: {e}")),
                }
            }
            Event::CalendarsFound(found) => {
                if let Some(form) = self.add_calendar.as_mut() {
                    form.finding = false;
                    let found = found.and_then(|all| if all.is_empty() { Err("This server has no calendars of events for this user.".to_owned()) } else { Ok(all) });
                    form.found = Some(found.map(|all| all.into_iter().map(|c| (c, true)).collect()));
                    let new: Vec<beavermail_carddav::Found> = match &form.found {
                        Some(Ok(found)) => found.iter().filter(|(c, _)| !self.settings.calendars.iter().any(|a| a.url == c.url)).map(|(c, _)| c.clone()).collect(),
                        _ => Vec::new(),
                    };
                    if matches!(&form.found, Some(Ok(found)) if found.len() == 1) && new.len() == 1 {
                        self.calendar_action(calendars::CalendarAction::Added(new));
                    }
                }
            }
            Event::BooksFound(found) => {
                if let Some(form) = self.add_book.as_mut() {
                    form.finding = false;
                    // Ticked: the usual wish is the one address book there is, or all of them.
                    let found = found.and_then(|books| if books.is_empty() { Err("This server has no address books for this user.".to_owned()) } else { Ok(books) });
                    // On: the user's own address books; those the server makes (read-only) are off.
                    form.found = Some(found.map(|books| books.into_iter().map(|b| { let on = b.writable; (b, on) }).collect()));
                    // A single new one is the one meant: added at once, without a second step.
                    let new: Vec<beavermail_carddav::Found> = match &form.found {
                        Some(Ok(found)) => found.iter().filter(|(b, _)| !self.settings.address_books.iter().any(|a| a.url == b.url)).map(|(b, _)| b.clone()).collect(),
                        _ => Vec::new(),
                    };
                    if matches!(&form.found, Some(Ok(found)) if found.len() == 1) && new.len() == 1 {
                        self.book_action(books::BookAction::Add(new));
                    }
                }
            }
            Event::Contacts(Ok(contacts)) => self.contacts = Arc::new(contacts),
            Event::Contacts(Err(_)) => {} // the fields only miss their suggestions
            Event::Unsubscribed(who, result) => match result {
                Ok(()) => self.notice = Some((format!("Unsubscribed from {who}. Mail already on its way may still arrive."), std::time::Instant::now())),
                Err(e) => self.error = Some(format!("Could not unsubscribe from {who}: {e}. Try the unsubscribe link in the message.")),
            },
            Event::Saved(result) => match result {
                Ok(Some(text)) => {
                    self.notice = Some((text, std::time::Instant::now()));
                    self.attach_selected.clear();
                }
                Ok(None) => {}
                Err(e) => self.error = Some(e),
            },
            Event::Sent(id, result, copy_error) => {
                if let Some(e) = copy_error.as_ref().filter(|_| result.is_ok()) {
                    self.error = Some(format!("The message was sent, but its copy could not be saved to Sent: {e}"));
                }
                if let Some((serial, draft)) = self.outbox_sending.remove(&id) {
                    self.sent_from_outbox(serial, draft, result);
                } else if self.compose.as_ref().is_some_and(|c| c.id == id) {
                    match result {
                        Ok(()) => {
                            if let Some(sent) = self.compose.take() {
                                self.delete_draft(&sent);
                            }
                            self.notice = Some(("Message sent".into(), std::time::Instant::now()));
                        }
                        // No connection: the message waits in the Outbox instead of failing.
                        Err(MailError::Offline(_)) => {
                            if let Some(mut compose) = self.compose.take() {
                                compose.sending = false;
                                self.queue(compose, None);
                            }
                        }
                        Err(e) => {
                            if let Some(compose) = &mut self.compose {
                                compose.sending = false;
                                compose.error = Some(e.to_string());
                            }
                        }
                    }
                }
            }
            _ => {} // answer for a folder or message that is no longer selected
        }
    }

    fn erase_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        if self.confirm_erase.is_empty() {
            return;
        }
        let count = self.confirm_erase.len();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_erase")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(if count == 1 { "Delete permanently?".to_owned() } else { format!("Delete {count} messages permanently?") });
            let what = if count == 1 { "This message" } else { "These messages" };
            ui.label(RichText::new(format!("{what} will be erased from the server and cannot be recovered.")).color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let erase = egui::Button::new(RichText::new("Delete permanently").color(t.on_accent)).fill(t.danger);
                confirm = ui.add(erase).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            let ids = std::mem::take(&mut self.confirm_erase);
            if self.message.is_some_and(|id| ids.contains(&id)) {
                self.message = None;
                self.body = None;
                self.in_tab = false;
            }
            ids.iter().for_each(|id| self.act(*id, Action::Erase));
            self.picked.clear();
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_erase.clear();
        }
    }

    /// Erases the Spam folder (`serial`, `index`) on the server. Its messages are kept as spam
    /// examples first, so the checks do not forget what spam looks like.
    fn empty_folder(&mut self, serial: u64, index: usize) {
        let Some(view) = self.view(serial) else { return };
        let (Some(source), Some((folder, role))) = (view.source.clone(), view.mailboxes.get(index).map(|m| (m.name.clone(), m.role))) else { return };
        let account = view.account.key();
        // What was in Spam stays an example of spam for the checks.
        if role == Role::Junk {
            match self.archive.messages(&account, &folder) {
                Ok(messages) => self.keep_examples(&account, &messages, true),
                Err(e) => {
                    self.error = Some(format!("Could not read the Spam folder: {e}"));
                    return;
                }
            }
        }
        if self.selected == Some((serial, index)) {
            self.messages.clear();
        } else if self.unified && self.unified_role == role {
            // Every account's folder of this kind: only this account's messages go.
            let origin = &self.origin;
            self.messages.retain(|m| origin.get(&m.id) != Some(&serial));
        }
        self.tabs.retain(|t| t.serial != serial || t.role != role);
        if let Some(mailbox) = self.accounts.iter_mut().find(|v| v.serial == serial).and_then(|v| v.mailboxes.get_mut(index)) {
            mailbox.total = 0;
            mailbox.unread = 0;
        }
        self.spawn(async move { Event::Done(serial, index, None, source.empty(role).await.map(drop)) });
    }

    /// Confirms emptying Spam or Trash, of one account or of several: nothing in it can be brought back afterwards.
    fn empty_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let targets = self.confirm_empty.clone();
        let Some(name) = targets.first().and_then(|&(serial, index)| emptied(self.view(serial)?.mailboxes.get(index)?.role)) else {
            self.confirm_empty.clear();
            return;
        };
        let count: usize = targets.iter().filter_map(|&(serial, index)| self.view(serial).and_then(|v| Some(self.archive.messages(&v.account.key(), &v.mailboxes.get(index)?.name).ok()?.len()))).sum();
        // The folders moved into the trash go with it, each with those inside it and their messages.
        let mut folders: Vec<(u64, String)> = Vec::new();
        let mut in_folders = 0;
        for &(serial, index) in &targets {
            if let Some(v) = self.view(serial)
                && let Some(trash) = v.mailboxes.get(index).filter(|m| m.role == Role::Trash)
            {
                let inside: Vec<&Mailbox> = v.mailboxes.iter().filter(|m| views::is_inside(&m.name, &trash.name, trash.delimiter)).collect();
                folders.extend(inside.iter().filter(|m| !inside.iter().any(|o| views::is_inside(&m.name, &o.name, trash.delimiter))).map(|m| (serial, m.name.clone())));
                in_folders += inside.iter().map(|m| m.total).sum::<u32>();
            }
        }
        let accounts: Vec<String> = targets.iter().filter_map(|&(serial, _)| Some(self.view(serial)?.account.address().to_owned())).collect();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_empty")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            let messages = format!("{} message{}", views::thousands(count as u32), if count == 1 { "" } else { "s" });
            ui.heading(match folders.len() {
                0 => format!("Delete {messages} in {name}?"),
                n => format!("Delete {messages} and {n} folder{} in {name}?", if n == 1 { "" } else { "s" }),
            });
            if accounts.len() > 1 {
                ui.add(egui::Label::new(RichText::new(format!("Of {}.", accounts.join(", "))).color(t.text_dim)).wrap());
            }
            let learn = if name == "Spam" { " The spam checks keep learning from them." } else { "" };
            let held = match in_folders {
                0 => String::new(),
                n => format!(" The folders hold {} message{} of their own.", views::thousands(n), if n == 1 { "" } else { "s" }),
            };
            ui.add(egui::Label::new(RichText::new(format!("They will be erased from the server and cannot be recovered.{held}{learn}")).color(t.text_dim)).wrap());
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let erase = egui::Button::new(RichText::new(format!("Empty {name}")).color(t.on_accent)).fill(t.danger);
                confirm = ui.add_enabled(count > 0 || !folders.is_empty(), erase).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            for &(serial, index) in &targets {
                if self.view(serial).and_then(|v| self.archive.messages(&v.account.key(), &v.mailboxes.get(index)?.name).ok()).is_some_and(|l| !l.is_empty()) {
                    self.empty_folder(serial, index);
                }
            }
            for (serial, folder) in folders {
                if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
                    self.spawn(async move { Event::FolderDeleted(serial, folder.clone(), source.delete_folder(&folder).await) });
                }
            }
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_empty.clear();
        }
    }

    /// Confirms moving everything held as suspicious to the server's Spam folder.
    fn spam_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        if !self.confirm_spam {
            return;
        }
        let ids: Vec<MessageId> = self.visible().iter().map(|m| m.id).collect();
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_spam")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(format!("Move {} message{} to Spam?", ids.len(), if ids.len() == 1 { "" } else { "s" }));
            ui.label(RichText::new("Their senders are marked as spam, so their mail goes there from now on. Any message can be brought back from the Spam folder with Not spam.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                confirm = views::primary_button(ui, t, "Move to Spam", true).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.move_to_spam(&ids);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_spam = false;
        }
    }

    /// Signs in to both servers as the form says, in the background, saving nothing; an edit with the
    /// password left empty tests the stored one.
    fn test_connection(&mut self, editing: Option<u64>) {
        let stored = editing.and_then(|serial| self.view(serial)).map(|v| v.account.clone());
        let work_offline = self.work_offline;
        let (archive, vault) = (self.archive.clone(), self.vault.clone());
        let Some(form) = (if editing.is_none() { self.form.as_mut() } else { self.account_edit.as_mut() }) else { return };
        if work_offline {
            form.error = Some("Go online to test the connection.".into());
            return;
        }
        let account = match form.build() {
            Ok(account) => account,
            Err(e) => return form.error = Some(e),
        };
        let passwords = (|| {
            let password = match (&stored, form.password.is_empty()) {
                (Some(old), true) => store::password(old)?,
                _ => form.password.clone(),
            };
            let smtp = match (account.smtp_user.is_some(), &stored, form.smtp_password.is_empty()) {
                (false, ..) => password.clone(),
                (true, _, false) => form.smtp_password.clone(),
                (true, Some(old), true) if old.smtp_user.is_some() => store::smtp_password(old)?,
                (true, ..) => return Err("enter the password for the outgoing server".to_owned()),
            };
            Ok((password, smtp))
        })();
        let (password, smtp_password) = match passwords {
            Ok(both) => both,
            Err(e) => return form.error = Some(e),
        };
        form.error = None;
        let tested = (account.clone(), form.password.clone(), form.smtp_password.clone());
        form.check = Some(Check { tested: tested.clone(), answers: None });
        let (mut imap, mut smtp) = (ImapSource::new(&account, password, archive, vault), SmtpSender::new(&account, smtp_password));
        if let Some(provider) = account.oauth {
            let token = store::oauth::token_source(provider, account.clone());
            (imap, smtp) = (imap.with_token(token.clone()), smtp.with_token(token));
        }
        self.spawn(async move {
            let said = |result: Result<(), MailError>| {
                result.map_err(|e| match e {
                    MailError::Auth(why) => format!("the server refused the sign-in: {why}"),
                    MailError::Offline(why) | MailError::Backend(why) => why,
                    e => e.to_string(),
                })
            };
            let imap = said(imap.check().await);
            Event::Checked(editing, Box::new(tested), imap, said(smtp.check().await))
        });
    }

    /// Opens the provider's sign-in in the browser and waits for it in the background; `editing`: for
    /// that account's page, else for Add account.
    fn sign_in(&mut self, provider: OAuthProvider, editing: Option<u64>) {
        self.cancel_sign_in();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.signing_in = Some(SignIn { provider, editing, cancel: cancel.clone() });
        self.spawn(async move {
            let flag = cancel.clone();
            let open = |url: &str| opener::open_browser(url).map_err(|e| format!("could not open the browser: {e}"));
            let result = tokio::task::spawn_blocking(move || store::oauth::authorize(provider, open, &flag)).await.unwrap_or_else(|e| Err(e.to_string()));
            Event::SignedIn(cancel, result)
        });
    }

    fn cancel_sign_in(&mut self) {
        if let Some(sign_in) = self.signing_in.take() {
            sign_in.stop();
        }
    }

    /// A sign-in ended: Add account adds the account; an account's page keeps the new token until Save changes.
    fn signed_in(&mut self, cancel: Arc<std::sync::atomic::AtomicBool>, result: Result<store::oauth::Grant, String>) {
        let Some(sign_in) = self.signing_in.take_if(|s| Arc::ptr_eq(&s.cancel, &cancel)) else { return }; // cancelled, or replaced by a newer one
        let form = match sign_in.editing {
            None => self.form.as_mut(),
            Some(serial) => self.account_edit.as_mut().filter(|f| f.editing == Some(serial)),
        };
        let Some(form) = form else { return }; // its dialog or page was closed
        let grant = match result {
            Ok(grant) => grant,
            Err(e) => return form.error = Some(e),
        };
        if form.editing.is_some() {
            if !grant.email.eq_ignore_ascii_case(&form.user) {
                return form.error = Some(format!("You signed in as {}, but this account is {}.", grant.email, form.user));
            }
            form.password = grant.refresh_token;
            form.error = None;
            return;
        }
        // A name typed in the dialog wins over the one the provider has.
        let mut filled = NewAccount::signed_in(sign_in.provider, grant);
        if !form.name.trim().is_empty() {
            filled.name = std::mem::take(&mut form.name);
        }
        let result = self.save_account(&filled);
        let Some(form) = self.form.as_mut() else { return };
        form.name = filled.name;
        match result {
            Ok(serial) => {
                self.form = None;
                if self.show_settings {
                    self.settings_page = SettingsPage::Account(serial);
                }
            }
            Err(e) => form.error = Some(e),
        }
    }

    /// The Add account dialog; an existing account is changed on its page in Settings.
    fn account_form(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else { return };
        let t = theme::Tokens::get(ctx);
        let (mut submit, mut cancel) = (false, false);
        let (mut start, mut stop, mut test) = (None, false, false);
        let waiting = self.signing_in.as_ref().filter(|s| s.editing.is_none()).map(|s| s.provider);
        let ready = form.ready();
        let modal = egui::Modal::new(egui::Id::new("add_account")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading("Add account");
            if let Some(provider) = waiting {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Finish signing in to {} in your browser.", provider.name()));
                });
                ui.add_space(12.0);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.button("Cancel").clicked();
                });
                return;
            }
            let providers: Vec<OAuthProvider> = [OAuthProvider::Google, OAuthProvider::Microsoft].into_iter().filter(|p| store::oauth::available(*p)).collect();
            if !providers.is_empty() {
                ui.add_space(8.0);
                for provider in providers {
                    if ui.add_sized([ui.available_width(), theme::CONTROL_H], egui::Button::new(format!("Sign in with {}", provider.name()))).clicked() {
                        start = Some(provider);
                    }
                }
                ui.add_space(12.0);
                ui.label(RichText::new("Or connect any other mailbox over IMAP and SMTP.").color(t.text_dim));
            } else {
                ui.label(RichText::new("Connect an existing mailbox over IMAP and SMTP.").color(t.text_dim));
            }
            ui.add_space(8.0);
            // Scrolls on a short window, so the buttons below stay in reach.
            egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 240.0).max(160.0)).show(ui, |ui| {
                form_grid(ui, "account_form", |ui| {
                    ui.label("Your name");
                    let name = ui.add(field(&mut form.name).hint_text("What recipients see beside your address"));
                    if std::mem::take(&mut form.focus_first) {
                        name.request_focus();
                    }
                    ui.end_row();
                    ui.label("Email");
                    ui.add(field(&mut form.email).hint_text("Address mail is sent from"));
                    ui.end_row();
                    login_rows(ui, &mut form);
                });
                server_sections(ui, &t, &mut form);
                ui.add_space(8.0);
                test = check_rows(ui, &t, &form);
            });
            if let Some(error) = &form.error {
                ui.add_space(8.0);
                ui.colored_label(t.danger, error);
            }
            ui.add_space(12.0);
            // A fixed-height row: a bare right-to-left layout would claim the rest of the window.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let add = egui::Button::new(RichText::new("Add account").color(t.on_accent)).fill(t.accent);
                let add = ui.add_enabled(ready, add).on_disabled_hover_text("User, password and IMAP server are required");
                submit = add.clicked() || (ready && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                cancel = ui.button("Cancel").clicked();
            });
        });
        let mut open = !(cancel || modal.should_close());
        if stop || (!open && waiting.is_some()) {
            self.cancel_sign_in();
        }
        if let Some(provider) = start {
            form.error = None;
            self.sign_in(provider, None);
        }
        if test && open {
            self.form = Some(form);
            self.test_connection(None);
            return;
        }
        if submit {
            match self.save_account(&form) {
                Ok(serial) => {
                    open = false;
                    if self.show_settings {
                        self.settings_page = SettingsPage::Account(serial);
                    }
                }
                Err(e) => form.error = Some(e),
            }
        }
        if open {
            self.form = Some(form);
        }
    }
}

impl BeaverApp {
    /// The slim bar at the bottom: what the app is doing with the server and how stored mail is protected.
    fn status_bar(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut total, mut done, mut running) = (0u32, 0u32, false);
        let mut activity = String::new();
        for view in &self.accounts {
            total += view.progress.total.load(Relaxed);
            done += view.progress.done.load(Relaxed);
            running |= view.progress.running.load(Relaxed);
            if activity.is_empty() {
                activity = view.progress.activity();
            }
        }
        let locked = self.vault.is_locked();
        let mode = self.vault.mode();
        let dim = |text: String| RichText::new(text).size(13.0).color(t.text_dim);
        let mut toggle_pause = false;
        let (mut go_online, mut go_offline) = (false, false);
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::symmetric(14, 5));
        egui::Panel::bottom("status").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.work_offline {
                    let mut waiting = String::new();
                    if self.outbox > 0 {
                        waiting += &format!("  ·  {} waiting to be sent", views::thousands(self.outbox as u32));
                    }
                    if !self.offline_changes.is_empty() {
                        let n = self.offline_changes.len();
                        waiting += &format!("  ·  {n} change{} waiting", if n == 1 { "" } else { "s" });
                    }
                    ui.label(RichText::new(format!("Working offline{waiting}")).size(13.0).color(t.text));
                    go_online = ui.small_button("Go online").clicked();
                } else if locked {
                    ui.label(RichText::new("Stored mail is locked. Downloads wait for the passphrase.").size(13.0).color(t.danger));
                } else if running && total > 0 && done < total {
                    ui.label(dim("Downloading mail".to_owned()));
                    let fraction = done as f32 / total as f32;
                    ui.add(egui::ProgressBar::new(fraction).desired_width(220.0).desired_height(8.0).corner_radius(4.0).fill(t.accent));
                    ui.label(dim(format!("{done} of {total}")));
                    let label = if self.downloads_paused { "Resume" } else { "Pause" };
                    if ui.small_button(label).clicked() {
                        toggle_pause = true;
                    }
                    if self.downloads_paused {
                        ui.label(dim("Paused".to_owned()));
                    }
                } else if running || self.syncing > 0 {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(dim(if activity.is_empty() { "Syncing…".to_owned() } else { format!("{activity}…") }));
                } else if self.accounts.iter().any(|v| v.offline.is_some()) {
                    let names: Vec<&str> = self.accounts.iter().filter(|v| v.offline.is_some()).map(|v| v.account.address()).collect();
                    let why: Vec<String> = self.accounts.iter().filter_map(|v| Some(format!("{}: {}", v.account.address(), v.offline.as_ref()?))).collect();
                    let who = if names.len() == self.accounts.len() { String::new() } else { format!(" ({})", names.join(", ")) };
                    let mut waiting = String::new();
                    if self.outbox > 0 {
                        waiting += &format!("  ·  {} waiting to be sent", views::thousands(self.outbox as u32));
                    }
                    if !self.offline_changes.is_empty() {
                        let n = self.offline_changes.len();
                        waiting += &format!("  ·  {n} change{} waiting", if n == 1 { "" } else { "s" });
                    }
                    ui.label(dim(format!("Offline{who}: showing mail stored on this computer{waiting}"))).on_hover_text(why.join("\n"));
                } else if self.outbox > 0 {
                    ui.label(dim(format!("{} message{} in the Outbox", views::thousands(self.outbox as u32), if self.outbox == 1 { "" } else { "s" })));
                } else if let Some(next) = self.scheduled.iter().filter_map(|(_, d)| d.send_at).min() {
                    let n = self.scheduled.len();
                    ui.label(dim(format!("{n} message{} scheduled, the next {}", if n == 1 { "" } else { "s" }, compose::when_text(next))));
                } else if let Some(why) = &self.draft_error {
                    ui.label(RichText::new(format!("Draft not saved on the server: {why}")).size(13.0).color(t.danger));
                } else {
                    ui.label(dim("Up to date".to_owned()));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let protection = match (locked, mode) {
                        (true, _) => "Locked",
                        (_, Mode::Off) => "Mail on this computer is not encrypted",
                        (_, Mode::System) => "Mail on this computer is encrypted",
                        (_, Mode::Passphrase) => "Mail on this computer is encrypted with your passphrase",
                    };
                    ui.label(dim(protection.to_owned()));
                    if !self.work_offline {
                        ui.add_space(12.0);
                        let link = egui::Button::new(dim("Work offline".to_owned())).frame(false);
                        go_offline = ui.add(link).on_hover_text("Stop using the network: sending and changes wait until you go online").clicked();
                    }
                });
            });
        });
        if go_online || go_offline {
            self.set_work_offline(go_offline);
        }
        if toggle_pause {
            self.downloads_paused = !self.downloads_paused;
            for view in &self.accounts {
                view.progress.paused.store(self.downloads_paused, Relaxed);
            }
        }
        if running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
        }
    }

    /// The short-lived message at the bottom centre, over the content so nothing moves: Undo while an
    /// action waits for the server, else a notice ("Message sent").
    fn toast(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        const NOTICE_SECS: f32 = 4.0;
        let mut undo = false;
        let show = |ctx: &egui::Context, add: &mut dyn FnMut(&mut egui::Ui)| {
            egui::Area::new(egui::Id::new("toast"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -TOAST_LIFT))
                .interactable(true)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).inner_margin(egui::Margin::symmetric(16, 8)).show(ui, |ui| ui.horizontal(|ui| add(ui)));
                });
        };
        if let Some(pending) = &self.pending {
            let left = pending.until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                self.send_pending();
            } else {
                ctx.request_repaint_after(left);
                let (text, keys) = (pending.text.clone(), Command::Undo.keys(ctx));
                show(ctx, &mut |ui| {
                    ui.label(RichText::new(&text).color(t.text));
                    ui.add_space(8.0);
                    undo = ui.button("Undo").clicked();
                    ui.label(RichText::new(&keys).size(12.5).color(t.text_dim));
                });
                if undo {
                    self.undo();
                }
                return; // the notice waits: one at a time
            }
        }
        if let Some((text, since)) = self.notice.clone() {
            let left = NOTICE_SECS - since.elapsed().as_secs_f32();
            if left <= 0.0 {
                self.notice = None;
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f32(left));
                show(ctx, &mut |ui| {
                    ui.label(RichText::new(&text).color(t.text));
                });
            }
        }
    }

    fn error_banner(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let Some(error) = self.error.clone() else { return };
        let banner = egui::Frame::new().fill(t.danger.gamma_multiply(0.14)).inner_margin(egui::Margin::symmetric(16, 10));
        egui::Panel::top("error").frame(banner).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(t.danger, error);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Dismiss").clicked() {
                        self.error = None;
                    }
                });
            });
        });
    }

    /// The rail at the window's left edge: Mail and Contacts, each a part of the app of its own.
    fn rail(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::symmetric(8, 12));
        egui::Panel::left("rail").frame(frame).resizable(false).exact_size(theme::CONTROL_H + 12.0).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            // Settings at the bottom, for every part of the app; the parts from the top.
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                if icons::button(ui, t, Icon::Settings, &Command::Settings.tip(ui.ctx()), true, self.show_settings).clicked() {
                    self.run(Command::Settings);
                }
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    for (section, icon, command, name) in [(Section::Mail, Icon::Mail, Command::Mail, "Mail"), (Section::Calendar, Icon::Calendar, Command::Calendar, "Calendar"), (Section::Contacts, Icon::Users, Command::Contacts, "Contacts")] {
                        let on = self.section == section;
                        let button = icons::button(ui, t, icon, name, true, on);
                        button.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, name));
                        if button.clicked() && !on {
                            self.run(command);
                        }
                    }
                });
            });
        });
    }

    /// The Contacts section: the address book on the left, the card of the one picked beside it.
    fn contacts_view(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let archive = self.archive.clone();
        if self.address_book.is_none() {
            self.contact_photos.clear();
        }
        let book = self.address_book.get_or_insert_with(|| people(&archive)).clone();
        let open = self.contact_pane.as_ref().map(|c| (c.book_card, c.data.address.clone()));
        let mut pick: Option<Result<i64, String>> = None;
        let mut book_action = None;
        let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for person in book.iter() {
            if let Person::Card(c, _) = person {
                *counts.entry(c.book.clone()).or_default() += 1;
            }
        }
        // Laid out as Mail: the sources on the left (main action on top, "Add …" at the bottom), the
        // list in the middle, the open card on the right.
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::same(12));
        egui::Panel::left("contact_books").frame(frame).default_size(260.0).size_range(210.0..=340.0).show(ui, |ui| {
            if views::primary_wide(ui, t, Icon::Plus, "New contact", None).on_hover_text("Add someone by their address").clicked() {
                self.new_contact = Some((String::new(), String::new()));
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                let add = egui::Button::image_and_text(Icon::Plus.image(icons::SIZE, t.text_dim), "Add address book");
                if ui.add_sized([ui.available_width(), theme::CONTROL_H], add).on_hover_text("Sync contacts with Fastmail, Nextcloud, iCloud or another CardDAV server").clicked() {
                    book_action = Some(books::BookAction::Open);
                }
                ui.add_space(4.0);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        theme::dense(ui, self.settings.density);
                        if let Some(action) = books::section(ui, t, &self.settings.address_books, &self.contacts_syncing, self.contacts_shown.as_deref(), &counts, book.len() as u32, &self.settings.folded) {
                            book_action = Some(action);
                        }
                    });
                });
            });
        });
        let mut action = None;
        let frame = egui::Frame::new().fill(t.surface).inner_margin(egui::Margin::same(28));
        egui::Panel::right("contact_card").frame(frame).default_size(470.0).size_range(360.0..=900.0).show(ui, |ui| match self.contact_pane.as_mut() {
            Some(card) => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    action = card::body(ui, t, card, false);
                });
            }
            None => {
                views::empty_state(ui, t, "No contact open", "Pick someone in the list to see their card, notes and conversations.", None);
            }
        });
        let frame = egui::Frame::new().fill(t.surface);
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let width = ui.available_width() - 12.0;
                ui.add(sized_field(&mut self.contact_query, width).hint_text("Search contacts"));
            });
            ui.add_space(6.0);
            let shown = self.contacts_shown.as_deref();
            let in_view = |p: &&Person| shown.is_none_or(|url| matches!(p, Person::Card(c, _) if c.book == url));
            let query = self.contact_query.trim();
            let found: Vec<&Person> = book.iter().filter(in_view).filter(|p| query.is_empty() || p.matches(query)).collect();
            if found.is_empty() {
                let (title, about) = match (book.iter().any(|p| in_view(&p)), shown.is_some()) {
                    (true, _) => ("No contact matches", "Try another name, address or number."),
                    (false, true) => ("No contacts here yet", "Add one with New contact while this address book is selected."),
                    (false, false) => ("No contacts yet", "Add one with New contact, save a sender from their card, or add an address book."),
                };
                views::empty_state(ui, t, title, about, None);
                return;
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, views::PERSON_ROW_H, found.len(), |ui, rows| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for person in &found[rows] {
                    // In one address book, where they are goes without saying: only the others are named.
                    let (name, line) = person.lines(&self.settings.address_books, shown.is_none());
                    let (key, here) = match person {
                        Person::Card(c, _) => (Ok(c.id), open.as_ref().is_some_and(|(card, _)| *card == Some(c.id))),
                        Person::Saved(c) => (Err(c.address.clone()), open.as_ref().is_some_and(|(card, address)| card.is_none() && *address == c.address)),
                    };
                    let photo = match person {
                        // Twice the size drawn, for sharp edges on a high-density screen.
                        Person::Card(c, _) => self.contact_photos.entry(c.id).or_insert_with(|| beavermail_carddav::vcard::details(&c.vcard).photo.and_then(|bytes| card::texture(ui.ctx(), &bytes, 72))).as_ref(),
                        Person::Saved(_) => None,
                    };
                    if views::person_row(ui, t, &name, &line, photo, here).clicked() && !here {
                        pick = Some(key);
                    }
                }
            });
        });
        if let Some(action) = book_action {
            self.book_action(action);
        }
        match pick {
            Some(Ok(id)) => self.contact_pane = self.load_book_card(id),
            Some(Err(address)) => self.contact_pane = self.load_card(&address, ""),
            None => {}
        }
        if let Some(action) = action {
            self.card_action(action, true);
        }
    }

    /// Accounts and their folders. Returns (folder chosen, account to remove, account to edit, add-account clicked).
    fn sidebar(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> SidebarOutcome {
        let (mut chosen, mut removed, mut edit, mut add, mut suspicious, mut recheck, mut empty) = (None, None, None, false, None, None, None);
        let (mut all, mut scheduled, mut all_suspicious, mut all_spam) = (false, false, false, false);
        let (mut recheck_all, mut empty_all) = (false, false);
        let mut fold = None;
        let mut dropped = None;
        let mut new_inside = None;
        let (mut rename, mut delete) = (None, None);
        // shortcut: only the Inboxes being shown have verdicts; the others keep the count of their last
        // check (All inboxes, shown at start, checks every one). Checking every account in the background
        // during sync would keep them all current.
        let mut held_now: std::collections::HashMap<u64, (u32, u32)> = std::collections::HashMap::new();
        // From the messages listed, as the list itself, so the count and the list never disagree; an
        // Inbox checked here with nothing held any more counts zero, not its last count.
        let checked: std::collections::HashSet<u64> = self.screening.keys().filter_map(|id| self.origin.get(id).copied()).collect();
        for (serial, _) in self.shown().into_iter().filter(|(s, _)| self.holding() && checked.contains(s)) {
            held_now.insert(serial, (0, 0));
        }
        for m in &self.messages {
            if let (Some(serial), Some(screened)) = (self.origin.get(&m.id), self.screening.get(&m.id)) {
                let held = u32::from(screened.place == screen::Place::Spam);
                let count = held_now.entry(*serial).or_default();
                *count = (count.0 + held, count.1 + held * u32::from(m.unread));
            }
        }
        self.held_counts.extend(held_now);
        let held_of = self.held_counts.clone();
        // The server counts held mail as unread in the Inbox; here it is counted under Suspicious instead.
        let hold = self.settings.hold_suspicious;
        let inbox_unread = |serial: u64, m: &Mailbox| if hold { m.unread.saturating_sub(held_of.get(&serial).map_or(0, |h| h.1)) } else { m.unread };
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::same(12));
        egui::Panel::left("accounts").frame(frame).default_size(260.0).size_range(210.0..=340.0).show(ui, |ui| {
            let can_send = !self.senders().is_empty();
            let why = if can_send { None } else { Some("Add an account to send mail") };
            if views::primary_wide(ui, t, Icon::Pencil, "New email", why).on_hover_text(Command::NewMessage.tip(ui.ctx())).clicked() && can_send {
                self.new_message();
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                add = ui.add_sized([ui.available_width(), theme::CONTROL_H], egui::Button::image_and_text(Icon::Plus.image(icons::SIZE, t.text_dim), "Add account")).clicked();
                ui.add_space(4.0);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        theme::dense(ui, self.settings.density);
                        if self.accounts.len() >= 2 {
                            let unread = self.accounts.iter().filter_map(|v| Some(inbox_unread(v.serial, v.mailboxes.iter().find(|m| m.role == Role::Inbox)?))).sum();
                            let inboxes = self.unified && self.unified_role == Role::Inbox;
                            // The menus of every account's folders: the one of an account's own, for all of them at once.
                            // Its (account, folder) answer stands for every account here.
                            let mut inbox_menu = |ui: &mut egui::Ui| {
                                let mut asked = None;
                                folder_menu(ui, Role::Inbox, (0, 0, 0), self.settings.hold_suspicious, &mut asked, &mut None, (0, 0));
                                recheck_all |= asked.is_some();
                            };
                            let row = views::folder_row(ui, t, "All inboxes", Role::Inbox, unread, inboxes && !self.show_suspicious).on_hover_text("The Inbox of every account, newest first");
                            all = row.clicked();
                            views::context_menu(&row, &mut inbox_menu);
                            if self.settings.hold_suspicious {
                                let held = self.accounts.iter().filter_map(|v| held_of.get(&v.serial)).map(|h| h.0).sum();
                                let row = views::suspicious_row(ui, t, held, inboxes && self.show_suspicious).on_hover_text("Mail from new senders that failed a check, every account's, held apart from the Inboxes");
                                all_suspicious = row.clicked();
                                views::context_menu(&row, &mut inbox_menu);
                            }
                            let spam_folders = || self.accounts.iter().filter_map(|v| Some((v, v.mailboxes.iter().find(|m| m.role == Role::Junk)?)));
                            let spam = spam_folders().map(|(_, m)| m.unread).sum();
                            let row = views::folder_row(ui, t, "Spam", Role::Junk, spam, self.unified && self.unified_role == Role::Junk).on_hover_text("The Spam folder of every account, newest first");
                            all_spam = row.clicked();
                            views::context_menu(&row, |ui| {
                                let listed = spam_folders().map(|(v, m)| self.archive.messages(&v.account.key(), &m.name).map_or(0, |l| l.len())).sum();
                                let mut asked = None;
                                folder_menu(ui, Role::Junk, (listed, spam_folders().map(|(_, m)| m.total).sum(), 0), self.settings.hold_suspicious, &mut None, &mut asked, (0, 0));
                                empty_all |= asked.is_some();
                            });
                        }
                        if !self.scheduled.is_empty() {
                            let row = views::icon_row(ui, t, "Scheduled", Icon::Calendar, self.scheduled.len() as u32, self.show_scheduled);
                            scheduled = row.on_hover_text("Messages to send later, every account's").clicked();
                        }
                        if self.accounts.len() >= 2 || !self.scheduled.is_empty() {
                            ui.add_space(8.0);
                        }
                        ui.label(RichText::new("ACCOUNTS").size(12.0).color(t.text_dim));
                        for (at, view) in self.accounts.iter().enumerate() {
                            // The colour that marks this account's mail in All inboxes.
                            let mut title = egui::text::LayoutJob::default();
                            if self.accounts.len() >= 2 {
                                title.append("● ", 0.0, egui::TextFormat::simple(egui::FontId::proportional(13.0), theme::account_color(at)));
                            }
                            title.append(view.account.address(), 0.0, egui::TextFormat::simple(egui::FontId::proportional(15.0), t.text));
                            // Seen with the account folded too: why its mail is not coming.
                            let small = |color| egui::TextFormat::simple(egui::FontId::proportional(12.5), color);
                            if view.error.is_some() {
                                title.append("not connected", 8.0, small(t.danger));
                            }
                            let header = egui::CollapsingHeader::new(title)
                                .icon(views::fold_arrow)
                                .id_salt(view.serial)
                                .default_open(!self.settings.folded.contains(&view.account.key()))
                                .show(ui, |ui| {
                                    if let Some(error) = &view.error {
                                        ui.add(egui::Label::new(RichText::new(error).size(13.0).color(t.danger)).wrap());
                                    } else if view.mailboxes.is_empty() && view.offline.is_none() && view.source.is_some() && !self.work_offline {
                                        // The server's list has not come yet, and none is stored from before.
                                        ui.horizontal(|ui| {
                                            ui.add(egui::Spinner::new().size(12.0));
                                            ui.label(RichText::new("Loading folders…").size(13.0).color(t.text_dim));
                                        });
                                    }
                                    // Suspicious is a row of its own after the Inbox and its subfolders, not one of them.
                                    let mut rows: Vec<(usize, usize, bool)> = views::folder_tree(&view.mailboxes).into_iter().map(|(i, depth)| (i, depth, false)).collect();
                                    if self.settings.hold_suspicious && let Some(inbox) = rows.iter().position(|&(i, ..)| view.mailboxes[i].role == Role::Inbox) {
                                        let after = rows[inbox + 1..].iter().position(|&(_, depth, _)| depth <= rows[inbox].1).map_or(rows.len(), |n| inbox + 1 + n);
                                        rows.insert(after, (rows[inbox].0, rows[inbox].1, true));
                                    }
                                    // A folded folder hides those inside it and counts their unread mail in its own.
                                    let open_key = |name: &str| folder_open_key(&view.account.key(), name);
                                    let mut hide_below: Option<usize> = None;
                                    let mut visible = Vec::with_capacity(rows.len());
                                    for (at, &(i, depth, held)) in rows.iter().enumerate() {
                                        if hide_below.is_some_and(|d| depth > d) {
                                            continue;
                                        }
                                        hide_below = None;
                                        let inside: Vec<usize> = rows[at + 1..].iter().take_while(|&&(_, d, _)| d > depth).map(|&(j, ..)| j).collect();
                                        let has_kids = !held && !inside.is_empty();
                                        let folded = has_kids && !self.settings.folded.contains(&open_key(&view.mailboxes[i].name));
                                        if folded {
                                            hide_below = Some(depth);
                                        }
                                        let hidden_unread: u32 = if folded { inside.iter().map(|&j| view.mailboxes[j].unread).sum() } else { 0 };
                                        visible.push((i, depth, held, has_kids.then_some(folded), hidden_unread));
                                    }
                                    for (i, depth, held, folding, hidden_unread) in visible {
                                        let mailbox = &view.mailboxes[i];
                                        let title = views::folder_title(&mailbox.name, mailbox.role, mailbox.delimiter);
                                        let here = self.selected == Some((view.serial, i));
                                        let holding = self.settings.hold_suspicious && mailbox.role == Role::Inbox;
                                        if held {
                                            let count = held_of.get(&view.serial).map_or(0, |h| h.0);
                                            let row = indented(ui, depth, |ui| views::suspicious_row(ui, t, count, here && self.show_suspicious)).on_hover_text("Mail from new senders that failed a check, held apart from the Inbox");
                                            if row.clicked() {
                                                suspicious = Some((view.serial, i));
                                            }
                                            views::context_menu(&row, |ui| folder_menu(ui, mailbox.role, (0, mailbox.total, 0), true, &mut recheck, &mut empty, (view.serial, i)));
                                            continue;
                                        }
                                        let tip = format!("{} message{}  ·  {} unread", views::thousands(mailbox.total), if mailbox.total == 1 { "" } else { "s" }, views::thousands(mailbox.unread));
                                        let unread = if mailbox.role == Role::Inbox { inbox_unread(view.serial, mailbox) } else { mailbox.unread } + hidden_unread;
                                        let selected = here && !(holding && self.show_suspicious);
                                        let row = indented(ui, depth, |ui| views::folder_row(ui, t, &title, mailbox.role, unread, selected)).on_hover_text(tip);
                                        // The arrow sits in the margin left of the row, drawn as the accounts' own.
                                        if let Some(folded) = folding {
                                            let side = ui.spacing().icon_width;
                                            let spot = egui::Rect::from_center_size(egui::pos2(row.rect.left() - 9.0, row.rect.center().y), vec2(side, side));
                                            let arrow = ui.interact(spot, ui.id().with(("fold", &mailbox.name)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                                            views::fold_arrow(ui, if folded { 0.0 } else { 1.0 }, &arrow);
                                            if arrow.on_hover_text(if folded { "Show the folders inside" } else { "Hide the folders inside" }).clicked() {
                                                fold = Some(open_key(&mailbox.name));
                                            }
                                        }
                                        if row.clicked() {
                                            chosen = Some((view.serial, i));
                                        }
                                        // Messages are dropped only on another folder of their own account.
                                        let takes = |d: &Dragged| d.0.iter().all(|id| self.serial_of(*id) == Some(view.serial) && self.archive.locate(*id).ok().flatten().is_none_or(|at| at.folder != mailbox.name));
                                        if row.dnd_hover_payload::<Dragged>().is_some_and(|d| takes(&d)) {
                                            ui.painter().rect_stroke(row.rect, egui::CornerRadius::same(theme::CONTROL_RADIUS), egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
                                        }
                                        if let Some(d) = row.dnd_release_payload::<Dragged>().filter(|d| takes(d)) {
                                            dropped = Some((d.0.clone(), view.serial, i));
                                        }
                                        // What emptying would erase: the messages listed here (worked out only when the menu is open).
                                        let listed = || self.archive.messages(&view.account.key(), &mailbox.name).map_or(0, |m| m.len());
                                        views::context_menu(&row, |ui| {
                                            // Only what can be done here is listed: no new folders in Trash or Spam, and the
                                            // Inbox and the other folders with a role keep their names (the app finds some by name).
                                            let can_add = !view.in_trash_or_spam(&mailbox.name);
                                            let own = mailbox.role == Role::Other;
                                            if can_add && ui.button("New folder…").clicked() {
                                                new_inside = Some((view.serial, i));
                                                ui.close();
                                            }
                                            if own && ui.button("Rename…").clicked() {
                                                rename = Some((view.serial, i));
                                                ui.close();
                                            }
                                            // In the trash there is nowhere left to put it: it goes for good, and says so.
                                            let in_trash = view.mailboxes.iter().any(|m| m.role == Role::Trash && views::is_inside(&mailbox.name, &m.name, mailbox.delimiter));
                                            let label = if in_trash { "Delete for good…" } else { "Delete folder…" };
                                            if own && ui.button(RichText::new(label).color(t.danger)).clicked() {
                                                delete = Some((view.serial, i));
                                                ui.close();
                                            }
                                            if (can_add || own) && matches!(mailbox.role, Role::Inbox | Role::Trash | Role::Junk) {
                                                ui.separator();
                                            }
                                            let inside = if mailbox.role == Role::Trash { view.mailboxes.iter().filter(|m| views::is_inside(&m.name, &mailbox.name, mailbox.delimiter)).count() } else { 0 };
                                            folder_menu(ui, mailbox.role, (listed(), mailbox.total, inside), self.settings.hold_suspicious, &mut recheck, &mut empty, (view.serial, i));
                                        });
                                    }
                                });
                            // Offline is a state, not news: a badge on the account, and why on a click.
                            if let Some(why) = view.offline.as_ref().filter(|_| view.error.is_none() && !self.work_offline) {
                                let galley = ui.painter().layout_no_wrap("offline".into(), egui::FontId::proportional(12.0), t.text_dim);
                                let row = header.header_response.rect;
                                let size = galley.size() + egui::vec2(16.0, 6.0);
                                let pill = egui::Rect::from_center_size(egui::pos2(row.right() - 8.0 - size.x / 2.0, row.center().y), size);
                                let badge = ui.interact(pill, ui.id().with(("offline", view.serial)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                                ui.painter().rect_filled(pill, egui::CornerRadius::same(11), if badge.hovered() { t.border } else { t.hover });
                                ui.painter().galley(pill.center() - galley.size() / 2.0, galley, t.text_dim);
                                egui::Popup::from_toggle_button_response(&badge).show(|ui| {
                                    ui.set_max_width(280.0);
                                    ui.label(RichText::new(format!("{} cannot be reached", view.account.imap.host)).size(14.0).strong().color(t.text));
                                    ui.add(egui::Label::new(RichText::new(why).size(13.0).color(t.text_dim)).wrap());
                                    ui.add_space(4.0);
                                    ui.add(egui::Label::new(RichText::new("The mail stored on this computer is shown meanwhile, and the server is asked again on its own.").size(13.0).color(t.text_dim)).wrap());
                                });
                            }
                            if header.header_response.clicked() {
                                fold = Some(view.account.key());
                            }
                            views::context_menu(&header.header_response, |ui| {
                                if ui.button("Account settings…").clicked() {
                                    edit = Some(view.serial);
                                    ui.close();
                                }
                                if ui.button("Remove account…").clicked() {
                                    removed = Some(view.serial);
                                    ui.close();
                                }
                            });
                        }
                        if self.accounts.is_empty() {
                            ui.label(RichText::new("No accounts yet").color(t.text_dim));
                        }
                    });
                });
            });
        });
        // Remembered for the next start; within a session egui keeps the state itself.
        if let Some(key) = fold {
            if !self.settings.folded.remove(&key) {
                self.settings.folded.insert(key);
            }
            self.save_settings();
        }
        SidebarOutcome { chosen, suspicious, recheck, empty, recheck_all, empty_all, removed, edit, add, all, scheduled, all_suspicious, all_spam, dropped, new_inside, rename, delete }
    }

    /// Search, refresh and the message list. Returns the message the user clicked and whether to add an account.
    fn message_list(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> (Option<MessageId>, bool) {
        let (mut opened, mut add, mut refresh) = (None, false, false);
        let (mut open_draft, mut delete_draft): (Option<String>, Option<String>) = (None, None);
        let mut scheduled_choice: Option<(u64, drafts::DraftData, views::DraftAction)> = None;
        let mut ask_spam = false;
        let (mut pinned, mut toggled, mut ranged): (Option<MessageId>, Option<MessageId>, Option<MessageId>) = (None, None, None);
        let mut clicked_hit: Option<(u64, Hit, bool)> = None;
        let (reveal, view) = (self.list_reveal.take(), self.list_view);
        let frame = egui::Frame::new().fill(t.surface);
        let list = egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if self.accounts.is_empty() {
                add = views::empty_state(ui, t, "Welcome to BeaverMail", "Add an account to start reading your mail.", Some("Add account"));
                return;
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let width = ui.available_width() - theme::CONTROL_H - 20.0;
                let focus = std::mem::take(&mut self.focus_search);
                views::search_box(ui, t, &mut self.query, "Search mail  ( / )", width, focus)
                    .on_hover_text(format!("Search the folder shown, as stored on this computer; in:all searches every folder.\nWords, \"a phrase\", from:ada, to:bob, subject:invoice, in:inbox, is:unread, has:attachment, after:2026-09-01, before:2026-10\n\nAll commands and their keys: {}", Command::Palette.keys(ui.ctx())));
                if self.syncing > 0 {
                    ui.add_sized([theme::CONTROL_H - 4.0, theme::CONTROL_H - 4.0], egui::Spinner::new().size(16.0)).on_hover_text("Syncing…");
                } else {
                    refresh = icons::button(ui, t, Icon::Refresh, "Refresh", true, false).clicked();
                }
            });
            let shown = self.visible();
            let can_move = self.show_suspicious && self.holding() && !shown.is_empty();
            if let Some(summary) = self.folder_summary() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new(summary).size(13.0).color(t.text_dim));
                    if can_move {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(12.0);
                            ask_spam = ui.button("Move all to Spam…").on_hover_text("Move every message held here to the server's Spam folder and mark their senders as spam").clicked();
                        });
                    }
                });
            }
            ui.add_space(6.0);
            if self.show_scheduled {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new("Scheduled  ·  sent at their time while BeaverMail is open").size(13.0).color(t.text_dim));
                });
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (serial, draft) in &self.scheduled {
                        let action = views::draft_row(ui, t, draft);
                        if !matches!(action, views::DraftAction::None) {
                            scheduled_choice = Some((*serial, draft.clone(), action));
                        }
                    }
                });
                return;
            }
            if self.searching() {
                if self.hits.is_empty() {
                    views::empty_state(ui, t, "No results", "Nothing stored on this computer matches. Try fewer words.", None);
                    return;
                }
                ui.spacing_mut().item_spacing.y = 0.0; // rows touch, so row `i` starts at `i` times the row height
                let area = list_area(reveal, view);
                let shown_rows = area.show_rows(ui, views::MESSAGE_ROW_H, self.hits.len(), |ui, range| {
                    for (serial, hit) in &self.hits[range] {
                        let id = hit.message.id;
                        let selected = if self.picked.len() >= 2 { self.picked.contains(&id) } else { self.message == Some(id) };
                        let row = views::message_row(ui, t, &hit.message, selected, Some(&hit.folder), None, 1, None);
                        row.dnd_set_drag_payload(Dragged(if self.picked.contains(&id) { self.picked.clone() } else { vec![id] }));
                        let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
                        let keep = row.double_clicked() || row.triple_clicked() || row.middle_clicked();
                        if row.clicked() && !keep && ctrl {
                            toggled = Some(id);
                        } else if row.clicked() && !keep && shift {
                            ranged = Some(id);
                        } else if keep || row.clicked() {
                            clicked_hit = Some((*serial, hit.clone(), keep));
                        }
                    }
                });
                self.list_view = (shown_rows.state.offset.y, shown_rows.inner_rect.height());
                return;
            }
            if !self.drafts.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new(format!("Drafts on this computer ({})", self.drafts.len())).size(13.0).family(theme::bold()).color(t.text_dim));
                });
                egui::ScrollArea::vertical().id_salt("drafts").max_height(views::MESSAGE_ROW_H * 2.6).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for draft in &self.drafts {
                        match views::draft_row(ui, t, draft) {
                            views::DraftAction::Open => open_draft = Some(draft.id.clone()),
                            views::DraftAction::Delete => delete_draft = Some(draft.id.clone()),
                            views::DraftAction::None => {}
                            // A scheduled one, shown among the drafts of the account it goes from.
                            action => scheduled_choice = self.selected.map(|(serial, _)| (serial, draft.clone(), action)),
                        }
                    }
                });
                ui.separator();
            }
            if shown.is_empty() {
                if !self.drafts.is_empty() {
                    return;
                }
                let (title, hint) = if self.show_suspicious && self.holding() {
                    ("Nothing suspicious", "Mail from new senders that fails a check is held here, apart from the Inbox.")
                } else if self.query.trim().is_empty() {
                    ("Nothing here", "This folder has no messages.")
                } else {
                    ("No results", "No loaded message matches your search.")
                };
                views::empty_state(ui, t, title, hint, None);
                return;
            }
            ui.spacing_mut().item_spacing.y = 0.0; // rows touch, so row `i` starts at `i` times the row height
            let area = list_area(reveal, view);
            let threaded = self.threaded();
            let latest = self.conversation_dates();
            let shown_rows = area.show_rows(ui, views::MESSAGE_ROW_H, shown.len(), |ui, range| {
                for message in &shown[range] {
                    let warning = self.correction(message.id).and_then(|(_, warning)| warning);
                    // A conversation's row: its latest message here, bold while any of it is unread.
                    let (count, unread) = if threaded { (self.conversations.members(message.id).len(), self.conversations.unread(message.id)) } else { (1, message.unread) };
                    let open = self.message.is_some_and(|id| if threaded { self.conversations.same(id, message.id) } else { id == message.id });
                    let selected = if self.picked.len() >= 2 { self.picked.contains(&message.id) } else { open };
                    // The date of the conversation's latest message, the one it is ordered by.
                    let received = latest.get(&message.id).map_or_else(|| message.received.clone(), |l| l.1.clone());
                    let summary = MessageSummary { unread, received, ..(*message).clone() };
                    let mark = self.origin.get(&message.id).and_then(|s| self.accounts.iter().position(|v| v.serial == *s)).filter(|_| self.unified).map(theme::account_color);
                    let row = views::message_row(ui, t, &summary, selected, None, warning, count, mark);
                    // The picked messages go together when one of them is dragged.
                    row.dnd_set_drag_payload(Dragged(if self.picked.contains(&message.id) { self.picked.clone() } else { vec![message.id] }));
                    let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
                    // A click just before the double click makes egui count three: that opens the tab too.
                    if row.double_clicked() || row.triple_clicked() || row.middle_clicked() {
                        pinned = Some(message.id);
                    } else if row.clicked() && ctrl {
                        toggled = Some(message.id);
                    } else if row.clicked() && shift {
                        ranged = Some(message.id);
                    } else if row.clicked() {
                        opened = Some(message.id);
                    }
                }
            });
            self.list_view = (shown_rows.state.offset.y, shown_rows.inner_rect.height());
        });
        let list = list.response.rect;
        if self.pane == Some(Pane::List) {
            pane_ring(ui, t, list);
        }
        if opened.is_some() || pinned.is_some() || toggled.is_some() || ranged.is_some() || clicked_hit.is_some() {
            self.pane = None; // a click in the list puts the keys back on it
        }
        if refresh {
            self.refresh();
        }
        self.confirm_spam |= ask_spam;
        if let Some(id) = pinned {
            self.open_message(id, true);
        }
        if let Some(id) = toggled {
            self.toggle_pick(id);
        }
        if let Some(id) = ranged {
            self.pick_range(id);
        }
        if let Some((serial, hit, pin)) = clicked_hit {
            if pin || self.settings.reading_pane {
                self.open_hit(serial, &hit, pin);
            } else {
                self.message = Some(hit.message.id); // selected, as in the folder's list
            }
        }
        if let Some(id) = open_draft {
            self.open_draft(&id);
        }
        if let Some((serial, draft, action)) = scheduled_choice {
            self.scheduled_action(serial, draft, action);
        }
        if let Some(id) = delete_draft
            && let Some((serial, _)) = self.selected
            && let Some(key) = self.view(serial).map(|v| v.account.key())
            && let Err(e) = self.forget_draft(serial, &key, &id, true)
        {
            self.error = Some(format!("Could not delete the draft: {e}"));
        }
        (opened, add)
    }

    /// The open message with its action bar. Returns the action the user chose.
    fn reader(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> ReaderOutcome {
        if self.showing_source() {
            self.source_page(ui, t);
            return ReaderOutcome::default();
        }
        let mut out = ReaderOutcome::default();
        let scroll = self.reader_scroll.take();
        // The open message's conversation, oldest first, when the list shows conversations.
        let conversation: Vec<MessageSummary> = match self.message.filter(|_| self.threaded()) {
            Some(id) => self.conversations.members(id).iter().filter_map(|m| self.summary_of(*m).cloned()).collect(),
            None => Vec::new(),
        };
        let whole = self.whole_conversation;
        let work_offline = self.work_offline;
        // The correction that makes sense for the tab's message; the reasons are the warnings shown above it.
        let correction = self.message.and_then(|id| self.correction(id)).map(|(choice, _)| choice);
        let (settings, view_modes, selected_files) = (self.settings.clone(), self.view_modes.clone(), self.attach_selected.clone());
        // What the calendars say about the invitations of the open messages, worked out before drawing.
        let carried: Vec<(MessageId, String)> = self
            .message
            .zip(self.body.as_ref())
            .into_iter()
            .map(|(id, b)| (id, &b.data))
            .chain(self.unfolded.iter().filter_map(|u| Some((u.id, &u.body.as_ref()?.as_ref().ok()?.data))))
            .filter_map(|(id, data)| Some((id, data.calendar.clone()?)))
            .collect();
        let invites: std::collections::HashMap<MessageId, (beavermail_carddav::ical::Calendar, calview::InviteInfo)> =
            carried.into_iter().map(|(id, ics)| (id, self.invite_info(&ics))).collect();
        // The message in the reader is its tab's: it may come from another folder than the one shown.
        let open = self.open_tab().map(|t| (t.message.clone(), t.role, t.serial));
        let server_host = open.as_ref().and_then(|(_, _, serial)| self.view(*serial)?.source.as_ref()).map(|s| s.auth_server()).unwrap_or_default();
        let role = open.as_ref().map_or(Role::Other, |(_, role, _)| *role);
        let current = open.map(|(message, ..)| message);
        let folded_anchor = self.folded_anchor;
        let frame = egui::Frame::new().fill(t.surface);
        let in_tab = self.in_tab;
        let picked = if in_tab { 0 } else { self.picked.len() };
        let picked_unread = self.picked.iter().any(|id| self.summary_of(*id).is_some_and(|m| m.unread));
        let folder_role = self.picked_role();
        let draw = |ui: &mut egui::Ui| {
            if picked >= 2 {
                out.bulk = views::picked_panel(ui, t, picked, picked_unread, folder_role, &mut out.move_to);
                return;
            }
            let Some(message) = current else {
                views::empty_state(ui, t, "No message selected", "Choose a message from the list to read it. Double-click to open it in a tab.", None);
                return;
            };
            // Answers go to the latest message of the conversation, as in every threaded client.
            let latest = conversation.last().map_or(message.id, |m| m.id);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let archived = role == Role::Archive;
                let ctx = ui.ctx().clone();
                let answers = [
                    (Icon::Reply, Command::Reply, Action::Reply),
                    (Icon::ReplyAll, Command::ReplyAll, Action::ReplyAll),
                    (Icon::Forward, Command::Forward, Action::Forward),
                ];
                for (icon, command, what) in answers {
                    if icons::button(ui, t, icon, &command.tip(&ctx), true, false).clicked() {
                        out.action = Some((latest, what));
                    }
                }
                ui.add_space(8.0);
                let key = |c: Command| c.keys(&ctx);
                let tools = [
                    (Icon::Archive, Command::Archive.tip(&ctx), Action::Archive, !archived),
                    (
                        if message.unread { Icon::MailOpen } else { Icon::Mail },
                        format!("{} ({})", if message.unread { "Mark as read" } else { "Mark as unread" }, key(Command::ToggleRead)),
                        Action::Unread(!message.unread),
                        true,
                    ),
                    (Icon::Trash, if role == Role::Trash { format!("Delete permanently ({})", key(Command::Trash)) } else { Command::Trash.tip(&ctx) }, Action::Trash, true),
                ];
                for (icon, tip, what, enabled) in tools {
                    let tip = if !enabled { "Already in the archive" } else { tip.as_str() };
                    if icons::button(ui, t, icon, tip, enabled, false).clicked() {
                        out.action = Some((message.id, what));
                    }
                }
                out.move_to |= icons::button(ui, t, Icon::Folder, &Command::MoveTo.tip(&ctx), true, false).clicked();
                // Drawn pressed in while pinned.
                let tip = format!("{} ({})", if message.pinned { "Unpin" } else { "Pin to the top of the list" }, key(Command::Pin));
                if icons::button(ui, t, Icon::Pin, &tip, true, message.pinned).clicked() {
                    out.action = Some((message.id, Action::Pin(!message.pinned)));
                }
                let tip = format!("{} ({})", if message.starred { "Remove the star" } else { "Star: a flag every mail app shows" }, key(Command::Star));
                if icons::button(ui, t, Icon::Star, &tip, true, message.starred).clicked() {
                    out.action = Some((message.id, Action::Star(!message.starred)));
                }
                // In Sent and Drafts the sender is the user.
                let blockable = !matches!(role, Role::Junk | Role::Sent | Role::Drafts) && !message.address.is_empty();
                let tip = match role {
                    _ if blockable => "Block sender…",
                    Role::Junk => "Already in Spam",
                    _ => "Your own mail",
                };
                if icons::button(ui, t, Icon::Ban, tip, blockable, false).clicked() {
                    out.block = Some(message.id);
                }
            });
            ui.add_space(6.0);
            ui.separator();
            let Some(body) = &mut self.body else {
                match self.body_failed.as_ref().filter(|(failed, _)| *failed == message.id) {
                    Some((_, why)) => out.retry = views::empty_state(ui, t, "This message could not be opened", why, Some("Try again")),
                    None => {
                        ui.centered_and_justified(|ui| ui.spinner());
                    }
                }
                return;
            };
            // One open message: who wrote it, what deserves a look, its attachments and its text. Every
            // open message of a conversation is drawn alike; `foldable`: its header closes it.
            let section = |ui: &mut egui::Ui, m: &MessageSummary, body: &mut OpenBody, anchor: bool, foldable: bool| -> Said {
                let mut said = Said::default();
                egui::Frame::new().stroke(egui::Stroke::new(1.0, t.border)).corner_radius(egui::CornerRadius::same(10)).inner_margin(egui::Margin::same(16)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let sense = if foldable { egui::Sense::click() } else { egui::Sense::hover() };
                    let (rect, header) = ui.allocate_exact_size(vec2(ui.available_width(), views::AVATAR), sense);
                    views::avatar(ui, t, &m.from, rect.left_center() + vec2(views::AVATAR / 2.0, 0.0), views::AVATAR);
                    let authenticity = sender::authenticity(&body.data.auth, &server_host, &m.address);
                    let suspicious = body.findings.iter().any(|f| f.level == screen::Level::Danger);
                    // Never for spam: leaving a list it sends only tells the spammer the address is read.
                    let way = unsubscribe::method(&body.data.unsubscribe, body.data.one_click, authenticity == sender::Authenticity::Verified)
                        .filter(|_| role != Role::Junk && !suspicious && !work_offline);
                    let mut text_right = rect.right();
                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(Layout::right_to_left(Align::Center)), |ui| {
                        let more = icons::button(ui, t, Icon::Ellipsis, "More: Reply all, Forward, View source", true, false);
                        egui::Popup::menu(&more).style(views::menu_look).show(|ui| {
                            for (label, what) in [("Reply all", Action::ReplyAll), ("Forward", Action::Forward)] {
                                if ui.button(label).clicked() {
                                    said.answer = Some(what);
                                }
                            }
                            if ui.button("View source").clicked() {
                                said.source = true;
                            }
                        });
                        if icons::button(ui, t, Icon::Reply, "Reply to this message", true, false).clicked() {
                            said.answer = Some(Action::Reply);
                        }
                        text_right = ui.min_rect().left() - 12.0;
                        if let Some(way) = way {
                            let tip = match &way {
                                unsubscribe::Method::OneClick(_) => format!("Ask {} to stop sending this mail", way.host()),
                                unsubscribe::Method::Mail(..) => format!("Write the email {} asks for to unsubscribe", way.host()),
                                unsubscribe::Method::Page(_) => format!("Open the unsubscribe page of {}", way.host()),
                            };
                            let button = ui.button("Unsubscribe").on_hover_text(tip);
                            text_right = button.rect.left() - 12.0;
                            if button.clicked() {
                                said.unsubscribe = Some((m.from.clone(), way));
                            }
                        }
                    });
                    let left = rect.left() + views::AVATAR + 12.0;
                    let detail = if m.address.is_empty() || m.address == m.from { m.received.clone() } else { format!("{}  ·  {}", m.address, m.received) };
                    let name = views::truncated(ui, &m.from, egui::FontId::proportional(15.5), t.text, text_right - left);
                    let date = views::truncated(ui, &detail, egui::FontId::proportional(13.0), t.text_dim, text_right - left);
                    // The name and the avatar open the sender's card; the rest of the header closes the message.
                    let who = egui::Rect::from_min_size(rect.min, vec2(left - rect.left() + name.size().x, views::AVATAR));
                    let open = ui.interact(who, ui.id().with(("sender_card", m.id.0)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open their card: name, notes, your conversations");
                    if open.clicked() {
                        said.card = Some((m.address.clone(), m.from.clone()));
                    }
                    if foldable && header.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        said.collapse = true;
                    }
                    ui.painter().galley(pos2(left, rect.top() + 3.0), name, t.text);
                    ui.painter().galley(pos2(left, rect.top() + 23.0), date, t.text_dim);
                    ui.add_space(4.0);
                    for finding in &body.findings {
                        let color = if finding.level == screen::Level::Danger { t.danger } else { t.text_dim };
                        views::note(ui, Icon::Spam, color, &finding.text, "");
                    }
                    // One line when all is well (the details on hover); each problem on a line of its own.
                    for (text, tip, tone) in views::security_notes(body.data.encryption.as_ref(), body.data.signature.as_ref()) {
                        let (icon, color) = match tone {
                            views::Tone::Good => (Icon::Lock, t.accent),
                            views::Tone::Quiet => (Icon::Lock, t.text_dim),
                            views::Tone::Warning => (Icon::Spam, t.danger),
                        };
                        views::note(ui, icon, color, &text, &tip);
                    }
                    // In every view, though only the original could load them: the user sees who tracks.
                    if let Some(line) = views::trackers_line(&body.trackers) {
                        views::note(ui, Icon::Ban, t.text_dim, &line, "Images that only tell the sender you opened this message. They are never loaded, even when you load the others.");
                    }
                    // What the server found: a fact next to the findings, not a reassurance ("paypa1.com"
                    // verifies too), so in the quiet colour whatever it says.
                    if let Some((line, tip)) = sender::checks_line(&body.data.auth, &server_host, &m.address) {
                        let icon = if authenticity == sender::Authenticity::Verified { Icon::ShieldCheck } else { Icon::Shield };
                        views::note(ui, icon, t.text_dim, &line, &tip);
                    }
                    if let Some(choice) = correction.filter(|_| anchor) {
                        let key = screen::sender_key(&m.address);
                        let who = key.trim_start_matches('@');
                        let (text, held, button, tip) = match choice {
                            Correction::NotSpam => ("Held apart from the Inbox as suspicious.".to_owned(), true, "Not spam", format!("Show mail from {who} in the Inbox from now on")),
                            // New means never written to, not never heard from: a newsletter can stay "new" for years.
                            Correction::Spam => (format!("You have never written to {}.", if key.starts_with('@') { format!("anyone at {who}") } else { who.to_owned() }), false, "Spam", format!("Move mail from {who} to the Spam folder, now and from now on")),
                            Correction::OutOfSpam => ("In the Spam folder.".to_owned(), true, "Not spam", format!("Move it back to the Inbox, and keep mail from {who} there from now on")),
                        };
                        ui.add_space(10.0);
                        // A new sender in the Inbox can be trusted from here, before anything goes wrong.
                        let trust_tip = format!("Mail from {who} always reaches the Inbox, unless your server says it is forged");
                        let offer = (choice == Correction::Spam).then_some(trust_tip.as_str());
                        match views::mark_bar(ui, t, &text, held, button, &tip, offer) {
                            Some(views::Mark::Correct) => said.mark = Some(choice),
                            Some(views::Mark::Trust) => said.trust = true,
                            None => {}
                        }
                    }
                    if let Some((calendar, info)) = invites.get(&m.id) {
                        ui.add_space(10.0);
                        let answerable = if suspicious { Err("Answering is off: this message failed the sender checks above.") } else { Ok(()) };
                        said.invite = calview::invitation_card(ui, t, calendar, info, answerable);
                    }
                    if !body.data.to.is_empty() || !body.data.cc.is_empty() {
                        ui.add_space(6.0);
                    }
                    for (label, list) in [("To", &body.data.to), ("Cc", &body.data.cc)].into_iter().filter(|(_, l)| !l.is_empty()) {
                        // A line cut short shows every address on hover.
                        ui.add(egui::Label::new(RichText::new(format!("{label}: {}", list.join(", "))).size(13.0).color(t.text_dim)).truncate());
                    }
                    // The calendar file of an invitation is its card above: not listed again.
                    let files: Vec<&beavermail_core::Attachment> = body
                        .data
                        .attachments
                        .iter()
                        .filter(|f| body.data.calendar.is_none() || !(f.mime.eq_ignore_ascii_case("text/calendar") || f.name.to_lowercase().ends_with(".ics")))
                        .collect();
                    if !files.is_empty() {
                        ui.add_space(10.0);
                        let many = files.len() > 1;
                        let ticked = selected_files.iter().filter(|(id, _)| *id == m.id.0).count();
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} attachment{}", files.len(), if many { "s" } else { "" })).size(13.0).color(t.text_dim));
                            if many {
                                let label = if ticked == 0 { "Save all…".to_owned() } else { format!("Save selected ({ticked})…") };
                                said.save_many = ui.button(label).clicked();
                            }
                        });
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            for file in &files {
                                if let Some(what) = views::attachment_chip(ui, t, file, many.then(|| selected_files.contains(&(m.id.0, file.index)))) {
                                    said.attachment = Some((file.index, what));
                                }
                            }
                        });
                    }
                    let stored = settings.mode_for(&m.address);
                    let mode = view_modes.get(&m.id).copied().unwrap_or(stored);
                    if body.simplified.is_some() {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Show as").size(13.0).color(t.text_dim));
                            for (label, value) in [("Text", DisplayMode::Plain), ("Simplified", DisplayMode::Simplified), ("Original", DisplayMode::Original)] {
                                if ui.selectable_label(mode == value, label).clicked() {
                                    said.mode = Some(value);
                                }
                            }
                            if mode != stored && !m.address.is_empty() && ui.button("Always for this sender").clicked() {
                                said.remember = Some(mode);
                            }
                        });
                    }
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(12.0);
                    if let (Some(source), DisplayMode::Original) = (&body.data.html, mode) {
                        let (width, scale) = (ui.available_width(), ui.ctx().pixels_per_point());
                        // A permission given to a sender never applies to a message that fails the server's checks.
                        // Working offline nothing is fetched, whatever was allowed.
                        let remote = !work_offline && (body.remote || (settings.allows_remote(&m.address) && !suspicious));
                        if !body.original.as_ref().is_some_and(|p| p.fits(width, scale, remote)) {
                            let drawing = std::time::Instant::now();
                            body.original = original::Page::new(ui.ctx(), source, width, scale, remote, &body.trackers);
                            log::debug!("message {}: original HTML drawn in {:.0?}", m.id.0, drawing.elapsed());
                        }
                        if body.has_remote {
                            match views::remote_bar(ui, t, remote, if suspicious { "" } else { &m.address }) {
                                Some(views::Remote::Once) => body.remote = true,
                                Some(views::Remote::Always) => said.allow_sender = Some(true),
                                Some(views::Remote::Block) => {
                                    body.remote = false;
                                    said.allow_sender = Some(false);
                                }
                                None => {}
                            }
                            ui.add_space(8.0);
                        }
                        if let Some(page) = &mut body.original {
                            said.url = page.show(ui);
                        }
                    } else {
                        let (blocks, fold) = match (&body.simplified, mode) {
                            (Some(simplified), DisplayMode::Simplified) => (simplified, body.folds.1),
                            _ => (&body.plain, body.folds.0),
                        };
                        said.url = views::reply_body(ui, t, blocks, fold, &mut body.quoted);
                    }
                });
                said
            };
            let mut heard: Vec<(MessageSummary, Said)> = Vec::new();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if let Some(scroll) = scroll {
                    let page = (ui.clip_rect().height() - 48.0).max(48.0);
                    // A positive delta moves the text down, that is, scrolls up; the area stops at its ends.
                    ui.scroll_with_delta(vec2(0.0, match scroll {
                        Scroll::Lines(n) => -48.0 * n,
                        Scroll::Pages(n) => -page * n,
                        Scroll::Edge(end) => if end { -1e7 } else { 1e7 },
                    }));
                }
                // A margin, not `ui.horizontal`: horizontal layouts do not wrap text, so a long
                // line would widen the whole panel.
                egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 0)).show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(760.0));
                    ui.add_space(20.0);
                    ui.heading(&message.subject);
                    ui.add_space(12.0);
                    if conversation.len() > 1 {
                        let open: Vec<MessageId> = (!folded_anchor).then_some(message.id).into_iter().chain(self.unfolded.iter().map(|u| u.id)).collect();
                        // The latest few closed ones, so a long conversation does not push the open ones out of sight.
                        let shown_from = if whole || conversation.len() <= 6 { 0 } else { conversation.len() - 5 };
                        let hidden = conversation[..shown_from].iter().filter(|m| !open.contains(&m.id)).count();
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} messages in this conversation", conversation.len())).size(13.0).color(t.text_dim));
                            if hidden > 0 && ui.button(format!("Show {hidden} earlier")).clicked() {
                                out.expand = true;
                            }
                        });
                        ui.add_space(8.0);
                        for (i, m) in conversation.iter().enumerate() {
                            if m.id == message.id && !folded_anchor {
                                heard.push((m.clone(), section(ui, m, body, true, true)));
                                ui.add_space(10.0);
                            } else if let Some(unfolded) = self.unfolded.iter_mut().find(|u| u.id == m.id) {
                                match &mut unfolded.body {
                                    Some(Ok(other)) => heard.push((m.clone(), section(ui, m, other, false, true))),
                                    // Still being read, or not readable: its row, with a word on why.
                                    state => {
                                        if views::thread_row(ui, t, m, false).on_hover_text("Close it").clicked() {
                                            heard.push((m.clone(), Said { collapse: true, ..Said::default() }));
                                        }
                                        match state {
                                            Some(Err(why)) => {
                                                ui.label(RichText::new(format!("It could not be opened: {why}")).size(13.0).color(t.danger));
                                            }
                                            _ => {
                                                ui.add(egui::Spinner::new().size(14.0));
                                            }
                                        }
                                    }
                                }
                                ui.add_space(10.0);
                            } else if i >= shown_from {
                                if views::thread_row(ui, t, m, false).on_hover_text("Open it").clicked() {
                                    out.unfold = Some(m.id);
                                }
                                ui.add_space(2.0);
                            }
                        }
                    } else {
                        heard.push((message.clone(), section(ui, &message, body, true, false)));
                    }
                    // As at the end of a conversation in every threaded client: answer its latest message.
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        for (icon, label, what) in [(Icon::Reply, "Reply", Action::Reply), (Icon::ReplyAll, "Reply all", Action::ReplyAll), (Icon::Forward, "Forward", Action::Forward)] {
                            if ui.add(egui::Button::image_and_text(icon.image(icons::SIZE, t.text), label)).clicked() {
                                out.action = Some((latest, what));
                            }
                        }
                    });
                    ui.add_space(28.0);
                });
            });
            for (m, said) in heard {
                out.take(&m, said);
            }
        };
        // A message tab has the window to itself; otherwise the reader is the pane on the right.
        if in_tab {
            egui::CentralPanel::default().frame(frame).show(ui, draw);
        } else {
            let reader = egui::Panel::right("reader").frame(frame).default_size(470.0).size_range(360.0..=900.0).show(ui, draw);
            if self.pane == Some(Pane::Reader) {
                pane_ring(ui, t, reader.response.rect);
            }
        }
        out
    }
}

impl eframe::App for BeaverApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // What waited for Undo is done now, or the mail would come back at the next start.
        // One that cannot reach the server waits for the next run instead.
        if let Some(pending) = self.pending.take() {
            let jobs: Vec<_> = pending.items.into_iter().filter_map(|(serial, index, action, m, _)| Some((self.view(serial)?.source.clone()?, serial, index, m.id, action))).collect();
            let mut unsent: Vec<(u64, usize, MessageId, Action)> = jobs.iter().map(|(_, serial, index, id, action)| (*serial, *index, *id, action.clone())).collect();
            self.runtime.block_on(async {
                let all = async {
                    for (source, _, _, id, action) in jobs {
                        if !matches!(perform(source, id, action).await, Err(MailError::Offline(_))) {
                            unsent.retain(|c| c.2 != id);
                        }
                    }
                };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(10), all).await;
            });
            if !unsent.is_empty() {
                self.offline_changes.extend(unsent);
                self.save_offline();
            }
        }
        let _ = std::fs::remove_dir_all(&self.open_dir); // the decrypted copies made to open attachments
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // A frame that keeps the window from answering goes in the diagnostic log.
        struct SlowFrame(std::time::Instant);
        impl Drop for SlowFrame {
            fn drop(&mut self) {
                if self.0.elapsed() > std::time::Duration::from_millis(100) {
                    log::warn!("a frame took {:.0?}", self.0.elapsed());
                }
            }
        }
        let _slow = SlowFrame(std::time::Instant::now());
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }
        views::set_link_params(ui.ctx(), &self.settings.link_params);
        // Address books at start, then every quarter of an hour (changes made here go at once).
        if self.contacts_synced.is_none_or(|at| at.elapsed() > std::time::Duration::from_secs(15 * 60)) {
            self.contacts_synced = Some(std::time::Instant::now());
            self.sync_contacts(None);
            self.sync_calendars(None);
        }
        // Closing the app with a message being written asks first, as Discard does. One being sent, or
        // waiting out Undo send, is kept as a draft on the way out without asking.
        if ui.ctx().input(|i| i.viewport().close_requested())
            && self.compose.as_ref().is_some_and(|c| !c.is_empty() && !c.sending)
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            (self.confirm_discard, self.quitting) = (true, true);
        }
        // Changes still on their way to the server are waited for in a dialog, rather than in a window
        // that stops answering on the way out.
        if ui.ctx().input(|i| i.viewport().close_requested()) && !self.confirm_discard && (self.pending.is_some() || !self.in_flight.is_empty()) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.send_pending();
            self.closing = true;
        }
        if self.settings.threads {
            self.conversations.update(&self.messages, &self.sent);
        }
        self.send_scheduled(ui.ctx());
        // The last scheduled message went (or was taken back): the view goes with its row.
        if self.show_scheduled && self.scheduled.is_empty() {
            match self.accounts.iter().find_map(|v| Some((v.serial, v.mailboxes.iter().position(|m| m.role == Role::Inbox)?))) {
                _ if self.accounts.len() >= 2 => self.show_all_inboxes(),
                Some((serial, inbox)) => self.select_mailbox(serial, inbox),
                None => self.show_scheduled = false,
            }
        }
        let t = theme::Tokens::get(ui.ctx());
        self.error_banner(ui, &t);
        self.status_bar(ui, &t);
        self.rail(ui, &t);
        // Out of Mail, its tabs, folders, list and reader give the window to the section.
        let contacts = self.section != Section::Mail;
        match self.section {
            Section::Contacts => self.contacts_view(ui, &t),
            Section::Calendar => self.calendar_section(ui, &t),
            Section::Mail => {}
        }

        if self.in_tab && self.open_tab().is_none() {
            self.show_mail(); // its tab went with its folder or account
        }
        if !contacts && self.tabs.iter().any(|t| t.pinned) {
            let mut event = None;
            let bar = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 0 });
            egui::Panel::top("tabs").frame(bar).show(ui, |ui| {
                event = tabs::tab_bar(ui, &t, &self.tabs, self.active().filter(|_| self.in_tab));
            });
            match event {
                Some(tabs::TabEvent::Mail) if self.in_tab => self.show_mail(),
                Some(tabs::TabEvent::Activate(at)) => self.activate(at),
                Some(tabs::TabEvent::Close(at)) => self.close_tab(at),
                _ => {}
            }
        }
        let SidebarOutcome { chosen, suspicious, recheck, empty, recheck_all, empty_all, removed, edit, mut add, all, scheduled, all_suspicious, all_spam, dropped, new_inside, rename, delete } =
            if self.in_tab || contacts { SidebarOutcome::default() } else { self.sidebar(ui, &t) };
        if let Some((serial, index)) = new_inside {
            let parent = self.view(serial).and_then(|v| v.mailboxes.get(index)).map(|m| m.name.clone());
            self.new_folder = Some(NewFolder { serial, parent, name: String::new(), busy: false, error: None, renaming: None });
        }
        if let Some((serial, index)) = rename
            && let Some(mailbox) = self.view(serial).and_then(|v| v.mailboxes.get(index))
        {
            let name = views::folder_title(&mailbox.name, mailbox.role, mailbox.delimiter);
            self.new_folder = Some(NewFolder { serial, parent: None, name, busy: false, error: None, renaming: Some(mailbox.name.clone()) });
        }
        if let Some((serial, index)) = delete {
            self.confirm_folder_delete = self.view(serial).and_then(|v| v.mailboxes.get(index)).map(|m| (serial, m.name.clone(), None));
        }
        if let Some((ids, serial, index)) = dropped {
            self.move_to_folder(ids, serial, index);
        }
        // What is being dragged follows the pointer.
        if let (Some(dragged), Some(at)) = (egui::DragAndDrop::payload::<Dragged>(ui.ctx()), ui.ctx().pointer_interact_pos()) {
            let n = dragged.0.len();
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            egui::Area::new(egui::Id::new("dragged")).order(egui::Order::Tooltip).fixed_pos(at + vec2(14.0, 10.0)).interactable(false).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(format!("Move {n} message{}", if n == 1 { "" } else { "s" })));
            });
        }
        if all {
            self.show_suspicious = false;
            self.show_all_inboxes();
        }
        if all_suspicious {
            if !self.unified || self.unified_role != Role::Inbox {
                self.show_all_inboxes();
            }
            self.show_suspicious = true;
            self.searched.clear(); // the same Inboxes, another side of them
        }
        if all_spam {
            self.show_suspicious = false;
            self.show_all(Role::Junk);
        }
        if scheduled {
            self.show_scheduled_list();
        }
        if let Some(serial) = edit {
            self.show_settings = true;
            self.leave_settings_page(Some(SettingsPage::Account(serial)));
        }
        if let Some((serial, index)) = chosen {
            self.show_suspicious = false;
            self.select_mailbox(serial, index);
        }
        if let Some((serial, index)) = suspicious {
            if self.selected != Some((serial, index)) {
                self.select_mailbox(serial, index);
            }
            self.show_suspicious = true;
            self.searched.clear();
        }
        // A full check: every message again, with who is known worked out afresh. Opening another
        // folder checks it all anyway, since its verdicts start empty.
        if let Some((serial, index)) = recheck {
            if self.selected == Some((serial, index)) {
                self.rescreen();
            } else {
                self.show_suspicious = false;
                self.select_mailbox(serial, index);
            }
        }
        if recheck_all {
            if !self.unified || self.unified_role != Role::Inbox {
                self.show_all_inboxes();
            }
            self.rescreen();
        }
        if empty.is_some() {
            self.confirm_empty = empty.into_iter().collect();
        }
        // Only the Spam folders with mail listed: as for one, never mail the user has not seen.
        if empty_all {
            self.confirm_empty = self
                .accounts
                .iter()
                .filter_map(|v| Some((v.serial, v.mailboxes.iter().position(|m| m.role == Role::Junk)?)))
                .filter(|&(serial, index)| self.view(serial).and_then(|v| self.archive.messages(&v.account.key(), &v.mailboxes[index].name).ok()).is_some_and(|l| !l.is_empty()))
                .collect();
        }
        self.confirm_remove = removed.or(self.confirm_remove);

        self.handle_keys(ui.ctx());
        // Without the reading pane only a message tab has a reader.
        let shows_reader = !contacts && (self.in_tab || self.settings.reading_pane);
        let ReaderOutcome { action, url, mode, remember, attachment, save_many, allow_sender, mark, bulk, move_to, unsubscribe, block, unfold, collapse, expand, trust, card, retry, source, invite } =
            if shows_reader { self.reader(ui, &t) } else { ReaderOutcome::default() };
        if retry && let Some(at) = self.active() {
            self.activate(at);
        }
        if let Some((address, name)) = card {
            self.open_card(&address, &name);
        }
        if let Some(id) = trust {
            self.trust(id);
        }
        self.whole_conversation |= expand;
        if let Some(id) = unfold {
            self.unfold(id);
        }
        if let Some(id) = collapse {
            if self.message == Some(id) {
                self.folded_anchor = true;
            } else {
                self.unfolded.retain(|u| u.id != id);
            }
        }
        match unsubscribe {
            // The compose window is the question: nothing goes out until Send.
            Some((_, unsubscribe::Method::Mail(to, subject, text))) => {
                let from = self.open_tab().map(|t| t.serial);
                self.start_compose(from.map(|f| compose::Compose::addressed(f, &to, &subject, &text)));
            }
            Some(asked) => self.confirm_unsubscribe = Some(asked),
            None => {}
        }
        self.confirm_block = block.or(self.confirm_block);
        if let Some(id) = source {
            self.view_source(id);
        }
        match invite {
            Some((_, calview::Invite::Open(url))) => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
            Some((id, choice)) => self.answer_invitation(id, choice),
            None => {}
        }
        if move_to && self.can(Command::MoveTo).is_ok() {
            self.run(Command::MoveTo);
        }
        if let Some(action) = bulk {
            self.request_all(self.targets(), action);
        }
        if let (Some(choice), Some(id)) = (mark, self.message) {
            self.correct(id, choice);
        }
        if let Some((id, index, what)) = attachment {
            self.use_attachment(id, index, what);
        }
        if let Some(id) = save_many {
            self.save_attachments(id, self.chosen_attachments(id));
        }
        if let Some(url) = url {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if let Some((id, mode)) = mode {
            self.view_modes.insert(id, mode);
        }
        if let Some((address, allow)) = allow_sender {
            self.settings.set_remote(&address, allow);
            self.save_settings();
        }
        if let Some((address, mode)) = remember {
            self.settings.set_sender_mode(&address, mode);
            self.view_modes.clear();
            self.save_settings();
        }
        let (opened, add_from_list) = if self.in_tab || contacts { (None, false) } else { self.message_list(ui, &t) };
        add |= add_from_list;

        // A click shows the message beside the list; without that pane it only selects, and a double
        // click opens a tab.
        if let Some(id) = opened {
            self.move_to(id);
        }
        if let Some((id, what)) = action {
            self.request(id, what);
        }
        self.erase_dialog(ui.ctx(), &t);
        self.spam_dialog(ui.ctx(), &t);
        self.empty_dialog(ui.ctx(), &t);
        self.open_dialog(ui.ctx(), &t);
        self.settings_window(ui.ctx(), &t);
        // After Settings, so the dialogs it opens show above it.
        self.account_form(ui.ctx());
        self.remove_dialog(ui.ctx(), &t);
        self.card_dialog(ui.ctx(), &t);
        self.card_delete_dialog(ui.ctx(), &t);
        self.new_contact_dialog(ui.ctx(), &t);
        self.new_folder_dialog(ui.ctx(), &t);
        self.folder_delete_dialog(ui.ctx(), &t);
        self.book_remove_dialog(ui.ctx(), &t);
        self.calendar_remove_dialog(ui.ctx(), &t);
        self.event_dialogs(ui.ctx(), &t);
        let added: Vec<String> = self.settings.calendars.iter().map(|c| c.url.clone()).collect();
        if let Some(action) = books::add_dialog(ui.ctx(), &t, &mut self.add_calendar, &added.iter().map(String::as_str).collect::<Vec<_>>(), &calendars::CALENDARS) {
            match action {
                books::BookAction::Find => self.find_calendars(),
                books::BookAction::Add(chosen) => self.calendar_action(calendars::CalendarAction::Added(chosen)),
                _ => {}
            }
        }
        let added: Vec<String> = self.settings.address_books.iter().map(|b| b.url.clone()).collect();
        if let Some(action) = books::add_dialog(ui.ctx(), &t, &mut self.add_book, &added.iter().map(String::as_str).collect::<Vec<_>>(), &books::BOOKS) {
            self.book_action(action);
        }
        if let Some(action) = encryption::dialogs(ui.ctx(), &t, &mut self.key_dialogs) {
            self.key_action(action);
        }
        self.unsubscribe_dialog(ui.ctx(), &t);
        self.palette_window(ui.ctx(), &t);
        self.toast(ui.ctx(), &t);
        self.block_dialog(ui.ctx(), &t);
        if let Some(code) = security::recovery_dialog(ui.ctx(), &t, &mut self.security) {
            self.save_recovery(code);
        }
        if self.locked()
            && let Some(request) = security::unlock_prompt(ui.ctx(), &t, &mut self.security, self.unlocking, self.vault.mode(), self.vault.has_recovery())
        {
            self.run_vault(request, ui.ctx());
        }
        if self.query != self.searched {
            self.run_search();
        }
        self.drop_files_on_the_app(ui.ctx(), &t);
        self.compose_window(ui.ctx(), &t);
        self.closing_dialog(ui.ctx(), &t);
        if add {
            self.form = Some(NewAccount::default());
        }
    }
}

/// Verdicts as the archive keeps them.
fn kept(verdicts: &std::collections::HashMap<MessageId, screen::Screened>) -> Vec<(MessageId, String)> {
    verdicts.iter().filter_map(|(id, v)| Some((*id, serde_json::to_string(v).ok()?))).collect()
}

/// The menu of a folder in the sidebar. Only the Inbox is checked for spam, so the check is offered
/// there and explained elsewhere; the Spam folder can be emptied.
/// The name of a folder that can be emptied: Spam and Trash.
fn emptied(role: Role) -> Option<&'static str> {
    match role {
        Role::Junk => Some("Spam"),
        Role::Trash => Some("Trash"),
        _ => None,
    }
}

/// The right-click menu of a folder, for what only some roles have (emptying Trash or Spam, checking the
/// Inbox for spam); `count`: how many of its messages are listed here, how many the
/// server says it holds, and how many folders are inside it (those in the trash go when it is emptied).
fn folder_menu(ui: &mut egui::Ui, role: Role, count: (usize, u32, usize), holding: bool, recheck: &mut Option<(u64, usize)>, empty: &mut Option<(u64, usize)>, folder: (u64, usize)) {
    if let Some(name) = emptied(role) {
        // Only what is listed is erased, never mail the user has not seen.
        let why = match count {
            (0, 0, 0) => format!("{name} is empty"),
            (0, _, 0) => format!("Open {name} first: only the messages listed there are erased"),
            _ => String::new(),
        };
        let what = if role == Role::Trash { "Delete every message and folder in Trash for good" } else { "Delete every message in Spam for good" };
        let button = ui.add_enabled(why.is_empty(), egui::Button::new(format!("Empty {name}…")));
        if button.on_hover_text(what).on_disabled_hover_text(why).clicked() {
            *empty = Some(folder);
            ui.close();
        }
    }
    // Only the Inbox is checked for spam.
    if role != Role::Inbox {
        return;
    }
    let why = (!holding).then_some("Holding suspicious mail is turned off in Settings");
    let check = ui.add_enabled(why.is_none(), egui::Button::new("Check all mail for spam"));
    let check = match why {
        Some(why) => check.on_disabled_hover_text(why),
        None => check.on_hover_text("Check every message again, not only new ones, and work out again who you know"),
    };
    if check.clicked() {
        *recheck = Some(folder);
        ui.close();
    }
}

/// Puts the standard label `$label<n>` in a message's labels, or takes it out.
fn set_label(labels: &mut Vec<String>, n: u8, on: bool) {
    let keyword = format!("$label{n}");
    labels.retain(|l| *l != keyword);
    if on {
        labels.push(keyword);
    }
}

/// The bytes of an attachment, through the mail source (a function so tasks can call it without a method).
async fn source_attachment(source: &ImapSource, id: MessageId, index: u32) -> Result<(String, Vec<u8>), MailError> {
    source.attachment(id, index).await
}

/// A button label with its shortcut letter (the one at `at`) underlined.
fn accelerated(label: &str, at: usize, color: egui::Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::proportional(14.0);
    for (i, c) in label.chars().enumerate() {
        let underline = if i == at { egui::Stroke::new(1.0, color) } else { egui::Stroke::NONE };
        job.append(&c.to_string(), 0.0, egui::text::TextFormat { font_id: font.clone(), color, underline, ..Default::default() });
    }
    job
}

fn main() -> eframe::Result {
    if std::env::args().any(|a| a == "--screen-dry-run") {
        if let Err(e) = screen::dry_run() {
            eprintln!("beavermail: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // Without a console (a Windows release build), a failure would close the app without a word: it is
    // shown in a dialog and kept in a log file.
    std::panic::set_hook(Box::new(|info| report(&format!("BeaverMail stopped because of an error:\n\n{info}"))));
    let options = || eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("BeaverMail").with_app_id("beavermail").with_inner_size([1280.0, 800.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/beavermail.png")).expect("the app's icon is a valid PNG")),
        // With vsync, Mesa's Wayland swap blocks the UI thread until the compositor sends a frame
        // callback, which it withholds for hidden windows; the compositor then reports the app as
        // not responding. egui only repaints on demand, so tearing is not a concern.
        glow_options: eframe::egui_glow::GlowConfiguration { vsync: false, ..Default::default() },
        ..Default::default()
    };
    let start = |options: eframe::NativeOptions| {
        eframe::run_native(
            "BeaverMail",
            options,
            Box::new(|cc| {
                egui_extras::install_image_loaders(&cc.egui_ctx);
                theme::apply(&cc.egui_ctx);
                // egui's 0.3 s is quicker than the systems' (0.4 to 0.5 s), and it is timed by frame, so a slow
                // frame after the first click could split a double click in two.
                cc.egui_ctx.options_mut(|o| o.input_options.max_double_click_delay = 0.5);
                Ok(Box::new(BeaverApp::new(cc.egui_ctx.clone())))
            }),
        )
    };
    // Windows: Direct3D first (also in software, without a graphics driver), OpenGL if that fails.
    #[cfg(windows)]
    let result = start(eframe::NativeOptions { renderer: eframe::Renderer::Wgpu, ..options() }).or_else(|_| start(options()));
    #[cfg(not(windows))]
    let result = start(options());
    if let Err(e) = &result {
        report(&format!("BeaverMail could not open its window:\n\n{e}\n\nThe graphics driver may lack OpenGL 2 (a virtual machine or a remote desktop often does)."));
    }
    result
}

/// Tells the user why the app stopped: on standard error, in `beavermail-error.log` in the app's data
/// folder, and in a dialog, which is the only one of the three seen without a console.
fn report(text: &str) {
    eprintln!("{text}");
    let log = store::data_dir().map(|dir| dir.join("beavermail-error.log"));
    let saved = log.as_ref().ok().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(path, format!("{}\n{text}\n", chrono::Local::now())).ok()?;
        Some(path.display().to_string())
    });
    let text = match saved {
        Some(path) => format!("{text}\n\nAlso written to {path}"),
        None => text.to_owned(),
    };
    rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("BeaverMail").set_description(&text).show();
}

/// The messages from `anchor` to `to` in list order `ids`, `anchor` first (where the next range starts).
fn range(ids: &[MessageId], anchor: MessageId, to: MessageId) -> Option<Vec<MessageId>> {
    let (a, b) = (ids.iter().position(|i| *i == anchor)?, ids.iter().position(|i| *i == to)?);
    let mut range: Vec<MessageId> = ids[a.min(b)..=a.max(b)].iter().copied().filter(|i| *i != anchor).collect();
    range.insert(0, anchor);
    Some(range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_in_source_ignores_case_and_keeps_each_matchs_place() {
        let lines: Vec<String> = ["Subject: Hello", "hello HELLO", "Ünïcode hello"].iter().map(|l| (*l).to_owned()).collect();
        assert_eq!(find_hits(&lines, "hello"), [(0, 9, 14), (1, 0, 5), (1, 6, 11), (2, 10, 15)]);
        assert_eq!(&lines[2][10..15], "hello", "a byte position, past letters of two bytes");
        assert!(find_hits(&lines, "").is_empty());
        assert!(find_hits(&lines, "absent").is_empty());
    }

    #[test]
    fn someone_in_two_address_books_shows_in_both_marked() {
        let store = Store::in_memory().unwrap();
        let book = |name: &str, url: &str| beavermail_core::AddressBook { name: name.into(), url: url.into(), user: "ada".into(), default: false, read_only: false };
        let books = [book("Contacts", "https://cloud.example.org/c/"), book("Contacts", "https://google.example.com/g/")];
        for (b, uid) in [(&books[0], "1"), (&books[1], "2")] {
            let text = beavermail_carddav::vcard::new_card(uid, "Bob Brown", &["bob@y.org".into()], "");
            store.add_card(&b.url, &format!("{}{uid}.vcf", b.url), &text, &beavermail_carddav::fields(&text)).unwrap();
        }
        store.save_contact("dora@z.org", "Dora", "").unwrap();
        let all = people(&store);
        assert_eq!(all.len(), 3);
        let lines: Vec<(String, String)> = all.iter().map(|p| p.lines(&books, true)).collect();
        assert_eq!(lines[0], ("Bob Brown".to_owned(), "bob@y.org  ·  Contacts (cloud.example.org)  ·  also in Contacts (google.example.com)".to_owned()));
        assert_eq!(lines[2], ("Dora".to_owned(), "dora@z.org  ·  on this computer".to_owned()));
        assert_eq!(all[1].lines(&books, false).1, "bob@y.org  ·  also in Contacts (cloud.example.org)", "in one address book, only the others are named");
    }

    #[test]
    fn a_range_runs_either_way_from_its_anchor() {
        let ids: Vec<MessageId> = (1..=5).map(MessageId).collect();
        assert_eq!(range(&ids, MessageId(2), MessageId(4)), Some(vec![MessageId(2), MessageId(3), MessageId(4)]));
        assert_eq!(range(&ids, MessageId(4), MessageId(2)), Some(vec![MessageId(4), MessageId(2), MessageId(3)]));
        assert_eq!(range(&ids, MessageId(3), MessageId(3)), Some(vec![MessageId(3)]));
        assert_eq!(range(&ids, MessageId(9), MessageId(3)), None, "an anchor no longer listed");
    }

    #[test]
    fn a_sign_in_fills_the_providers_servers() {
        let grant = store::oauth::Grant { email: "ada@gmail.com".into(), name: None, refresh_token: "r".into() };
        let account = NewAccount::signed_in(OAuthProvider::Google, grant).build().unwrap();
        assert_eq!((account.user.as_str(), account.address(), account.imap.host.as_str(), account.smtp.port), ("ada@gmail.com", "ada@gmail.com", "imap.gmail.com", 587));
        assert_eq!(account.oauth, Some(OAuthProvider::Google));
        assert!(!account.save_sent, "the provider files sent mail itself");
    }

    #[test]
    fn a_test_result_stands_only_for_the_form_it_tested() {
        let mut form = filled();
        let tested = (form.build().unwrap(), form.password.clone(), String::new());
        form.check = Some(Check { tested, answers: Some((Ok(()), Err("bad".into()))) });
        assert!(form.current_check().is_some());
        form.imap_port = "993".into();
        assert!(form.current_check().is_none(), "another port: not what was tested");
        form.imap_port = "143".into();
        form.password = "other".into();
        assert!(form.current_check().is_none(), "another password either");
    }

    #[test]
    fn the_outgoing_server_may_have_its_own_login() {
        let mut form = filled();
        assert_eq!(form.build().unwrap().smtp_login(), "ada@example.org", "the same login by default");
        form.smtp_own = true;
        assert!(form.build().is_err(), "its user is needed then");
        form.smtp_user = " relay-ada ".into();
        let account = form.build().unwrap();
        assert_eq!((account.smtp_user.as_deref(), account.smtp_login(), account.user.as_str()), (Some("relay-ada"), "relay-ada", "ada@example.org"));
        assert_eq!(NewAccount::editing(7, &account).build().unwrap(), account, "kept through an edit");
        form.oauth = Some(OAuthProvider::Google);
        assert_eq!(form.build().unwrap().smtp_user, None, "a provider's sign-in covers both servers");
    }

    fn filled() -> NewAccount {
        NewAccount { user: "ada@example.org".into(), password: "pw".into(), imap_host: "mail.example.org".into(), ..NewAccount::default() }
    }

    #[test]
    fn the_login_is_the_address_when_it_looks_like_one() {
        assert_eq!(filled().build().unwrap().address(), "ada@example.org");
    }

    #[test]
    fn a_plain_login_needs_an_explicit_address() {
        let mut form = filled();
        form.user = "ada".into();
        assert!(form.build().is_err());
        form.email = "ada@example.org".into();
        assert_eq!(form.build().unwrap().address(), "ada@example.org");
        form.email = "not an address".into();
        assert!(form.build().is_err());
    }

    #[test]
    fn editing_keeps_the_password_when_left_empty() {
        let account = filled().build().unwrap();
        let mut form = NewAccount::editing(7, &account);
        assert!(form.ready(), "no password needed while editing");
        assert_eq!(form.build().unwrap(), account);
        form.imap_host.clear();
        assert!(!form.ready());
    }

    #[test]
    fn the_list_scrolls_only_as_far_as_needed_to_show_a_row() {
        let h = views::MESSAGE_ROW_H;
        assert_eq!(reveal_offset(2, 0.0, 5.0 * h), None, "already in view");
        assert_eq!(reveal_offset(9, 0.0, 5.0 * h), Some(5.0 * h), "below: the row ends at the bottom edge");
        assert_eq!(reveal_offset(1, 4.0 * h, 5.0 * h), Some(h), "above: the row starts at the top edge");
    }

    #[test]
    fn a_trusted_sender_typed_by_hand_is_an_address_or_an_organisation() {
        assert_eq!(trusted_key(" Bob@Shop.Example "), Some("bob@shop.example".into()));
        assert_eq!(trusted_key("@mail.shop.example"), Some("@shop.example".into()), "the organisation, as a mark from a message");
        for bad in ["bob", "bob@", "@", "@localhost", "a b@x.org", "bob@.org"] {
            assert_eq!(trusted_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_account_page_knows_when_it_has_unsaved_changes() {
        let account = filled().build().unwrap();
        let mut form = NewAccount::editing(7, &account);
        assert!(!form.changed(&account), "a page just opened has nothing to save");
        form.signature = "Ada".into();
        assert!(form.changed(&account));
        let mut form = NewAccount::editing(7, &account);
        form.password = "new".into();
        assert!(form.changed(&account), "a new password is a change");
        let mut form = NewAccount::editing(7, &account);
        form.imap_port = "not a port".into();
        assert!(form.changed(&account), "an invalid form is never dropped silently");
    }
}
