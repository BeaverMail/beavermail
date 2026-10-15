// SPDX-License-Identifier: AGPL-3.0-only

//! BeaverMail desktop shell: accounts with their folders, a message list and a reader.
//! Accounts come from `accounts.toml` and the system credential store (see
//! `beavermail-accounts`). Mail calls run on a background runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

mod attachments;
mod compose;
mod drafts;
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
mod views;

use beavermail_accounts as store;
use beavermail_core::{Account, DisplayMode, Hit, Keep, MailError, MailSender, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Progress, Role, Server, Settings, SyncReport, Tls};
use beavermail_imap::ImapSource;
use beavermail_smtp::{SmtpSender, render_draft};
use beavermail_store::Store;
use beavermail_vault::{Mode, SystemStore, Vault};
use eframe::egui::{self, Align, Layout, RichText, pos2, vec2};

use crate::icons::Icon;
use tokio::runtime::Runtime;

/// Result of a background mail call. Tagged with the account's serial, so answers
/// for an account that was removed meanwhile are dropped.
enum Event {
    /// The folders of an account. The flag is true when the answer came from the server.
    Mailboxes(u64, Result<Vec<Mailbox>, MailError>, bool),
    /// The background download of an account ended.
    Downloaded(u64, Result<(), MailError>),
    /// The server says something changed in the Inbox of an account.
    Changed(u64),
    /// Search results from one account: (search number, account, hits).
    Found(u64, u64, Result<Vec<Hit>, MailError>),
    /// One sync of the selected folder finished; more history may remain.
    Synced(u64, usize, Result<SyncReport, MailError>),
    Messages(u64, usize, Result<Vec<MessageSummary>, MailError>),
    Body(u64, MessageId, Result<MessageBody, MailError>),
    /// The checks' verdicts on the Inbox loaded as (account, folder index).
    Screened(u64, usize, Result<std::collections::HashMap<MessageId, screen::Screened>, MailError>),
    /// A write (read flag, archive, trash) finished; the UI already showed its result.
    Done(u64, usize, Result<(), MailError>),
    /// The message in the compose window was sent, or the server refused it.
    /// Second field: why the copy in Sent could not be saved, when the message itself went out.
    Sent(Result<(), MailError>, Option<String>),
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
}

enum VaultOutcome {
    Applied(Mode, Result<(), MailError>),
    Unlocked(Result<(), MailError>),
    Reset(Result<(), MailError>),
}

#[derive(Clone, Copy)]
enum Action {
    Unread(bool),
    Archive,
    Trash,
    /// To the server's Spam folder.
    Spam,
    /// From the Spam folder back to the Inbox.
    NotSpam,
    Erase,
    Reply,
    ReplyAll,
    Forward,
}

impl Action {
    /// Whether the message leaves the folder being shown.
    fn removes(self) -> bool {
        matches!(self, Self::Archive | Self::Trash | Self::Spam | Self::NotSpam | Self::Erase)
    }
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
    /// "Empty Spam" was chosen in this (account, Spam folder index)'s menu.
    empty: Option<(u64, usize)>,
    removed: Option<u64>,
    edit: Option<u64>,
    add: bool,
}

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
    /// What deserves a second look about the message, the most serious first.
    findings: Vec<screen::Finding>,
}

impl OpenBody {
    fn new(data: MessageBody, findings: Vec<screen::Finding>) -> Self {
        let plain = html::plain_blocks(&data.text);
        let simplified = data.html.as_deref().map(html::blocks);
        let has_remote = data.html.as_deref().is_some_and(html::has_remote);
        Self { data, plain, simplified, original: None, remote: false, has_remote, findings }
    }
}

/// What the user did in the reader this frame.
struct ReaderOutcome {
    action: Option<(MessageId, Action)>,
    /// A link that was clicked, cleaned of tracking parameters.
    url: Option<String>,
    /// A display mode picked for this message only.
    mode: Option<DisplayMode>,
    /// A display mode to remember for the sender of this message.
    remember: Option<DisplayMode>,
    /// An attachment to open or save: its position and what to do.
    attachment: Option<(u32, views::ChipAction)>,
    /// "Save all" or "Save selected" was pressed.
    save_many: bool,
    /// Remote content may always be loaded for the sender of this message.
    allow_sender: Option<bool>,
    /// The spam correction the user chose for the open message.
    mark: Option<Correction>,
    /// A command for all the messages picked in the list.
    bulk: Option<Action>,
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
    /// The background download, shared with the status bar.
    progress: Arc<Progress>,
    /// Set to stop the Inbox watcher; `watching` says whether one is running.
    watch_stop: Arc<std::sync::atomic::AtomicBool>,
    watching: bool,
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
    save_sent: bool,
    keep: Keep,
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
            save_sent: true,
            keep: Keep::default(),
            error: None,
            focus_first: true,
        }
    }
}

impl NewAccount {
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
            save_sent: account.save_sent,
            keep: account.keep,
            focus_first: true,
            ..Self::default()
        }
    }

    /// Whether the required fields are filled in.
    fn ready(&self) -> bool {
        let password = self.editing.is_some() || !self.password.is_empty();
        !self.user.trim().is_empty() && !self.imap_host.trim().is_empty() && password
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
        Ok(Account {
            name: if self.name.trim().is_empty() { user.to_owned() } else { self.name.trim().to_owned() },
            user: user.to_owned(),
            email: email.to_owned(),
            imap: Server { host: imap_host.to_owned(), port: port(&self.imap_port, "IMAP")?, tls: self.imap_tls },
            smtp: Server { host: smtp_host.to_owned(), port: port(&self.smtp_port, "SMTP")?, tls: self.smtp_tls },
            save_sent: self.save_sent,
            keep: self.keep,
        })
    }
}

const FORM_WIDTH: f32 = 460.0;
/// Text field width in the account form: the form minus its label column and spacing.
const FIELD_WIDTH: f32 = 310.0;

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
    /// The Spam folder (account, index) the user asked to empty, until they confirm.
    confirm_empty: Option<(u64, usize)>,
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
    /// The user paused the background downloads.
    downloads_paused: bool,
    /// An attachment that runs code, waiting for the user to say yes to opening it.
    confirm_open: Option<(u32, String)>,
    /// Attachments ticked in the reader, to save together.
    attach_selected: std::collections::BTreeSet<u32>,
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
    view_mode: Option<DisplayMode>,
    /// Open messages: the reading pane's and those in tabs; the one shown is `message`.
    tabs: Vec<tabs::Tab>,
    /// A message tab fills the window, rather than the mail (folders, list and reading pane).
    in_tab: bool,
    /// Where the checks put each message of the Inbox being shown; empty until they have run.
    screening: std::collections::HashMap<MessageId, screen::Screened>,
    /// The selected account's suspicious mail is shown instead of its Inbox.
    show_suspicious: bool,
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
            confirm_empty: None,
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
            notice: None,
            settings: Settings::default(),
            archive: Arc::new(archive),
            vault: Arc::new(vault),
            syncing: 0,
            show_settings: false,
            downloads_paused: false,
            confirm_open: None,
            attach_selected: std::collections::BTreeSet::new(),
            open_dir: store::data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("open"),
            security: security::Form::default(),
            vault_job: None,
            unlock_skipped: false,
            unlocking: false,
            view_mode: None,
            tabs: Vec::new(),
            in_tab: false,
            screening: std::collections::HashMap::new(),
            show_suspicious: false,
            checking: false,
        };
        app.error = archive_error.or(vault_error);
        let _ = std::fs::remove_dir_all(&app.open_dir); // copies of attachments from an earlier run
        match store::load_settings() {
            Ok(settings) => app.settings = settings,
            Err(e) => app.error = Some(e),
        }
        match store::load() {
            Ok(accounts) => accounts.into_iter().for_each(|a| app.add_view(a)),
            Err(e) => app.error = Some(e),
        }
        app
    }

    /// The checks of [`screen::findings`] on an opened message; none when what they need cannot be read.
    fn findings(&self, serial: u64, id: MessageId, body: &MessageBody) -> Vec<screen::Finding> {
        let (Some(view), Some(message)) = (self.view(serial), self.summary_of(id)) else { return Vec::new() };
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_owned()).collect();
        match screen::Known::load(&self.archive, &view.account, &own, &self.settings) {
            Ok(known) => screen::findings(&known, &message.from, &message.address, body, &view.account.imap.host),
            Err(_) => Vec::new(),
        }
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
    fn add_view(&mut self, account: Account) {
        self.insert_view(account, None);
    }

    /// Shows an account, at `position` or at the end, and returns its serial.
    fn insert_view(&mut self, account: Account, position: Option<usize>) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        let (source, sender, error) = match store::password(&account) {
            Ok(password) => {
                let sender = Arc::new(SmtpSender::new(&account, password.clone()));
                (Some(Arc::new(ImapSource::new(&account, password, self.archive.clone(), self.vault.clone()))), Some(sender), None)
            }
            Err(e) => (None, None, Some(e)),
        };
        if let Some(source) = source.clone() {
            // The archive answers at once; the server's list follows and replaces it.
            let cached = source.clone();
            self.spawn(async move { Event::Mailboxes(serial, cached.cached_mailboxes().await, false) });
            self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await, true) });
        }
        let progress = Arc::new(Progress::default());
        progress.paused.store(self.downloads_paused, std::sync::atomic::Ordering::Relaxed);
        let view = AccountView { serial, account, source, sender, mailboxes: Vec::new(), error, progress, watch_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)), watching: false };
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
        let (Some(source), Some(mailbox)) = (view.source.clone(), view.mailboxes.get(index)) else { return };
        let name = mailbox.name.clone();
        self.selected = Some((serial, index));
        self.refresh_drafts();
        // The reading pane belonged to the list being left; tabs stay.
        self.tabs.retain(|t| t.pinned);
        self.in_tab = false;
        self.picked.clear();
        self.message = None;
        self.body = None;
        self.messages.clear();
        self.screening.clear();
        self.checking = false;
        let local = source.clone();
        let folder = name.clone();
        self.spawn(async move { Event::Messages(serial, index, local.messages(&folder).await) });
        self.start_sync(serial, index, source, name);
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
        let listed = self.selected.zip(self.messages.iter().find(|m| m.id == id)).map(|((serial, _), m)| tabs::Tab { serial, message: m.clone(), role: self.role(), pinned: pin });
        let tabbed = || self.tabs.iter().find(|t| t.message.id == id).map(|t| tabs::Tab { serial: t.serial, message: t.message.clone(), role: t.role, pinned: pin });
        if let Some(tab) = listed.or_else(tabbed) {
            self.open(tab);
        }
    }

    /// Shows `tab`'s message, from any folder: in the reading pane, or when pinned, in its tab.
    fn open(&mut self, tab: tabs::Tab) {
        self.picked.clear();
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
        if self.active() != Some(at) {
            self.activate(at);
        }
        if !judging && unread {
            self.act(id, Action::Unread(false));
        }
    }

    /// Shows tab `at` in the reader. Its message is loaded again: from this computer when stored, so
    /// switching is quick without keeping every open message in memory.
    fn activate(&mut self, at: usize) {
        let Some(tab) = self.tabs.get(at) else { return };
        let (serial, id) = (tab.serial, tab.message.id);
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        self.in_tab = tab.pinned;
        self.message = Some(id);
        self.body = None;
        self.view_mode = None;
        self.attach_selected.clear();
        self.spawn(async move { Event::Body(serial, id, source.body(id).await) });
    }

    /// Back to the mail, with the message that was in the reading pane.
    fn show_mail(&mut self) {
        match self.tabs.iter().position(|t| !t.pinned) {
            Some(pane) => self.activate(pane),
            None => {
                self.in_tab = false;
                self.message = None;
                self.body = None;
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
        self.tabs.iter().position(|t| t.message.id == id && t.pinned == self.in_tab)
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
    }

    /// The account of message `id`: its tab's or search result's, which may not be the folder shown's.
    fn serial_of(&self, id: MessageId) -> Option<u64> {
        let tab = self.tabs.iter().find(|t| t.message.id == id).map(|t| t.serial);
        let hit = || self.hits.iter().find(|(_, h)| h.message.id == id).map(|(s, _)| *s);
        tab.or_else(hit).or(self.selected.map(|(s, _)| s))
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
        let index = self.selected.filter(|(s, _)| *s == serial).map_or(usize::MAX, |(_, i)| i);
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        let pos = self.messages.iter().position(|m| m.id == id);
        if let Some(pos) = pos {
            let was_unread = self.messages[pos].unread;
            let unread_change = match action {
                Action::Unread(unread) => {
                    self.messages[pos].unread = unread;
                    i32::from(unread) - i32::from(was_unread)
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
        self.spawn(async move {
            let result = match action {
                Action::Unread(unread) => source.set_unread(id, unread).await,
                Action::Archive => source.archive(id).await,
                Action::Trash => source.trash(id).await,
                Action::Spam => source.move_to(id, Role::Junk).await,
                Action::NotSpam => source.move_to(id, Role::Inbox).await,
                Action::Erase => source.erase(id).await,
                Action::Reply | Action::ReplyAll | Action::Forward => Ok(()),
            };
            Event::Done(serial, index, result)
        });
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
                    self.select_message(next);
                }
            }
        }
    }

    /// "1,240 messages · 3 unread · 980 stored offline" for the folder being shown.
    fn folder_summary(&self) -> Option<String> {
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
        let checked = self.holding() && !self.screening.is_empty();
        let want = if self.show_suspicious { screen::Place::Spam } else { screen::Place::Inbox };
        self.messages
            .iter()
            .filter(|m| !ours.contains(&m.message_id))
            .filter(|m| if checked { self.screening.get(&m.id).is_some_and(|s| s.place == want) } else { !self.show_suspicious })
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
        // The verdicts kept from before, read at once so held mail never shows in the Inbox.
        if self.screening.is_empty()
            && let Some(folder) = view.mailboxes.get(index).map(|f| f.name.clone())
            && let Ok(kept) = self.archive.verdicts(&account.key(), &folder)
        {
            self.screening.extend(kept.into_iter().filter_map(|(id, json)| Some((id, serde_json::from_str(&json).ok()?))));
        }
        let fresh: Vec<MessageSummary> = self.messages.iter().filter(|m| !self.screening.contains_key(&m.id)).cloned().collect();
        if fresh.is_empty() {
            return;
        }
        // Mail already held is part of the campaigns the new mail is compared with.
        let held: Vec<MessageSummary> = self.messages.iter().filter(|m| self.screening.get(&m.id).is_some_and(|s| s.place == screen::Place::Spam)).cloned().collect();
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_owned()).collect();
        let (archive, settings) = (self.archive.clone(), self.settings.clone());
        self.checking = true;
        self.spawn(async move {
            let verdicts = screen::Known::load(&archive, &account, &own, &settings).and_then(|known| {
                let verdicts: std::collections::HashMap<MessageId, screen::Screened> = fresh.iter().map(|m| m.id).zip(screen::screen_all(&source, &known, &settings, &account.imap.host, &fresh, &held)).collect();
                archive.set_verdicts(&kept(&verdicts))?;
                Ok(verdicts)
            });
            Event::Screened(serial, index, verdicts)
        });
    }

    /// Messages of the Inbox being shown that are held as suspicious.
    fn held(&self) -> usize {
        self.screening.values().filter(|s| s.place == screen::Place::Spam).count()
    }

    /// Checks the whole Inbox being shown again, after the setting or a mark was forgotten, or when
    /// asked from its menu: the kept verdicts are dropped first.
    fn rescreen(&mut self) {
        if let Some((serial, index)) = self.selected
            && let Some(view) = self.view(serial)
            && let Some(folder) = view.mailboxes.get(index)
            && let Err(e) = self.archive.clear_verdicts(&view.account.key(), &folder.name)
        {
            self.error = Some(format!("Could not reset the spam checks: {e}"));
        }
        self.screening.clear();
        self.show_suspicious &= self.settings.hold_suspicious;
        if let Some((serial, index)) = self.selected {
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
        let sources: Vec<(u64, Arc<ImapSource>)> = self.accounts.iter().filter_map(|v| Some((v.serial, v.source.clone()?))).collect();
        for (serial, source) in sources {
            let query = query.clone();
            self.spawn(async move { Event::Found(seq, serial, source.search(&query, 200).await) });
        }
    }

    /// A search result was clicked: show its folder with the message open.
    /// Shows a search result where it is, without leaving the search: its folder need not be the one shown.
    fn open_hit(&mut self, serial: u64, hit: &Hit, pin: bool) {
        let Some(role) = self.view(serial).and_then(|v| v.mailboxes.iter().find(|m| m.name == hit.folder)).map(|m| m.role) else { return };
        self.open(tabs::Tab { serial, message: hit.message.clone(), role, pinned: pin });
    }

    /// Asks the server again for the selected folder and its account's folder list.
    fn refresh(&mut self) {
        let Some((serial, index)) = self.selected else { return };
        let Some(view) = self.view(serial) else { return };
        let (Some(source), Some(mailbox)) = (view.source.clone(), view.mailboxes.get(index)) else { return };
        let name = mailbox.name.clone();
        let folders = source.clone();
        self.spawn(async move { Event::Mailboxes(serial, folders.mailboxes().await, true) });
        self.start_sync(serial, index, source, name);
        self.start_download(serial);
    }

    /// Accounts that can send, as (serial, address).
    fn senders(&self) -> Vec<(u64, String)> {
        self.accounts.iter().filter(|v| v.sender.is_some()).map(|v| (v.serial, v.account.address().to_owned())).collect()
    }

    /// Opens the compose window from the selected account, or the first one that can send.
    fn new_message(&mut self) {
        if self.compose.is_some() {
            return;
        }
        let senders = self.senders();
        let from = self.selected.map(|(s, _)| s).filter(|s| senders.iter().any(|(x, _)| x == s)).or_else(|| senders.first().map(|(s, _)| *s));
        self.start_compose(from.map(compose::Compose::new));
    }

    /// Shows a compose window, remembering which account's drafts it belongs to.
    fn start_compose(&mut self, compose: Option<compose::Compose>) {
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
        match compose::window(ctx, t, &mut compose, &senders, self.settings.rich_outgoing) {
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
            compose::Outcome::Send => {
                if let Some(view) = self.view(compose.from) {
                    let (account, sender, source) = (view.account.clone(), view.sender.clone(), view.source.clone());
                    if let Some(sender) = sender {
                        let mut message = compose.outgoing(account.address(), self.settings.rich_outgoing);
                        let files = compose.file_list();
                        let vault = self.vault.clone();
                        compose.sending = true;
                        compose.error = None;
                        self.spawn(async move {
                            // The copies are read now, off the window's thread, as they can be large.
                            match tokio::task::spawn_blocking(move || attachments::read_files(&vault, &files)).await {
                                Ok(Ok(read)) => message.attachments = read,
                                Ok(Err(e)) => return Event::Sent(Err(MailError::Backend(e)), None),
                                Err(e) => return Event::Sent(Err(MailError::Backend(e.to_string())), None),
                            }
                            let result = sender.send(message).await;
                            let copy_error = match (&result, source) {
                                (Ok(raw), Some(source)) if account.save_sent => source.save_sent(raw.clone()).await.err().map(|e| e.to_string()),
                                _ => None,
                            };
                            Event::Sent(result.map(|_| ()), copy_error)
                        });
                    }
                }
            }
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
            let (mut discard, mut keep) = (false, false);
            let modal = egui::Modal::new(egui::Id::new("confirm_discard")).show(ctx, |ui| {
                ui.set_width(FORM_WIDTH * 0.8);
                ui.heading("Discard this message?");
                ui.label(RichText::new("What you wrote will be lost.").color(t.text_dim));
                ui.add_space(12.0);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                    let button = egui::Button::new(RichText::new("Discard").color(t.on_accent)).fill(t.danger);
                    discard = ui.add(button).clicked();
                    keep = ui.button("Keep editing").clicked();
                });
            });
            if discard {
                self.confirm_discard = false;
                self.delete_draft(&compose);
                return;
            }
            if keep || modal.should_close() {
                self.confirm_discard = false;
            }
        }
        self.compose = Some(compose);
    }

    /// The stored mail is protected by a passphrase that has not been entered, and the prompt is showing.
    fn locked(&self) -> bool {
        self.vault.is_locked() && !self.unlock_skipped
    }

    /// What the user did to an attachment chip.
    fn use_attachment(&mut self, index: u32, what: views::ChipAction) {
        let name = self.body.as_ref().and_then(|b| b.data.attachments.iter().find(|a| a.index == index)).map(|a| a.name.clone());
        match what {
            views::ChipAction::Toggle => {
                if !self.attach_selected.remove(&index) {
                    self.attach_selected.insert(index);
                }
            }
            views::ChipAction::SaveAs => self.save_attachments(vec![index]),
            views::ChipAction::Open => {
                if let Some(name) = name.filter(|n| attachments::is_risky(n)) {
                    self.confirm_open = Some((index, name));
                } else {
                    self.fetch_attachment(index);
                }
            }
        }
    }

    /// The ticked attachments, or all of them when none is ticked.
    fn chosen_attachments(&self) -> Vec<u32> {
        let all = self.body.iter().flat_map(|b| b.data.attachments.iter().map(|a| a.index));
        if self.attach_selected.is_empty() { all.collect() } else { self.attach_selected.iter().copied().collect() }
    }

    fn fetch_attachment(&mut self, index: u32) {
        let (Some(serial), Some(id)) = (self.open_tab().map(|t| t.serial), self.message) else { return };
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        self.spawn(async move { Event::Attachment(source.attachment(id, index).await) });
    }

    /// Asks where to save, then saves: one attachment into a file of the user's choosing, several into
    /// a folder (names that exist get a number, nothing is overwritten).
    fn save_attachments(&mut self, indexes: Vec<u32>) {
        let (Some(serial), Some(id)) = (self.open_tab().map(|t| t.serial), self.message) else { return };
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        let names: Vec<(u32, String)> = indexes.iter().filter_map(|i| self.body.as_ref()?.data.attachments.iter().find(|a| a.index == *i)).map(|a| (a.index, attachments::safe_name(&a.name))).collect();
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
        let Some((index, name)) = self.confirm_open.clone() else { return };
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
            self.fetch_attachment(index);
        }
        if open || cancel || modal.should_close() {
            self.confirm_open = None;
        }
    }

    /// The sender's address of the open message.
    fn current_address(&self) -> Option<String> {
        self.open_tab().map(|t| t.message.address.clone())
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
            security::Request::Reset => {
                self.unlocking = true;
                self.runtime.spawn_blocking(move || {
                    let _ = tx.send(Event::Vault(VaultOutcome::Reset(vault.reset().and_then(|()| archive.reset(vault.derived_key("index").as_deref())))));
                    ctx.request_repaint();
                });
            }
        }
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
            VaultOutcome::Unlocked(Ok(())) => self.security.clear_unlock(),
            VaultOutcome::Unlocked(Err(MailError::Auth(_))) => self.security.unlock_error = Some("That passphrase does not open the stored mail.".into()),
            VaultOutcome::Unlocked(Err(e)) => self.security.unlock_error = Some(e.to_string()),
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
        let mut changed = false;
        let mut rescreen = false;
        let mut vault_request = None;
        let mut unlock_again = false;
        let (mode, locked, job) = (self.vault.mode(), self.vault.is_locked(), self.vault_job.clone());
        let modal = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading("Settings");
            ui.add_space(8.0);
            egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 220.0).max(160.0)).auto_shrink([false, true]).show(ui, |ui| {
            ui.label(RichText::new("Reading").strong());
            ui.label(RichText::new("Most unwanted mail and tracking arrives as HTML, so messages start as text.").color(t.text_dim));
            ui.add_space(6.0);
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
            if !self.settings.remote_senders.is_empty() {
                ui.add_space(12.0);
                ui.label(RichText::new("Senders that may load remote content").strong());
                let mut forget = None;
                for address in &self.settings.remote_senders {
                    ui.horizontal(|ui| {
                        ui.label(address);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
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
            ui.add_space(12.0);
            ui.label(RichText::new("Suspicious mail").strong());
            if ui.checkbox(&mut self.settings.hold_suspicious, "Hold suspicious mail from new senders apart from the Inbox").changed() {
                changed = true;
                rescreen = true;
            }
            ui.label(RichText::new("Mail from people you know is never held. Nothing moves on the server.").size(13.0).color(t.text_dim));
            if !self.settings.marked.is_empty() {
                ui.add_space(6.0);
                let mut forget = None;
                for (key, not_spam) in &self.settings.marked {
                    ui.horizontal(|ui| {
                        let who = key.strip_prefix('@').map_or_else(|| key.clone(), |d| format!("everyone at {d}"));
                        ui.label(format!("{who}  ·  {}", if *not_spam { "not spam" } else { "spam" }));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Forget").on_hover_text("Let the checks decide again").clicked() {
                                forget = Some(key.clone());
                            }
                        });
                    });
                }
                if let Some(key) = forget {
                    self.settings.set_mark(&key, None);
                    changed = true;
                    rescreen = true;
                }
            }
            ui.add_space(12.0);
            ui.label(RichText::new("Writing").strong());
            if ui.checkbox(&mut self.settings.rich_outgoing, "Write in Markdown and send an HTML version too").changed() {
                changed = true;
            }
            ui.label(RichText::new("Bold, lists, links and quotes. The plain-text part is always sent, readable as written. Off: plain text only.").size(13.0).color(t.text_dim));
            if !self.settings.sender_display.is_empty() {
                ui.add_space(12.0);
                ui.label(RichText::new("Senders with their own setting").strong());
                let mut forget = None;
                for (address, mode) in &self.settings.sender_display {
                    ui.horizontal(|ui| {
                        let label = match mode { DisplayMode::Original => "Original HTML", DisplayMode::Simplified => "Simplified HTML", DisplayMode::Plain => "Plain text" };
                        ui.label(format!("{address}  ·  {label}"));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
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
            ui.add_space(16.0);
            ui.separator();
            ui.add_space(8.0);
            vault_request = security::section(ui, t, &mut self.security, mode, locked, job.as_deref());
            if locked && ui.button("Unlock…").clicked() {
                unlock_again = true;
            }
            });
            ui.add_space(14.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Done").clicked() {
                    self.show_settings = false;
                }
            });
        });
        if modal.should_close() {
            self.show_settings = false;
        }
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

    /// Role of the folder being shown.
    fn role(&self) -> Role {
        let (serial, index) = self.selected.unzip();
        serial.zip(index).and_then(|(s, i)| self.view(s)?.mailboxes.get(i)).map_or(Role::Other, |m| m.role)
    }

    /// Runs a user action; trashing from the trash asks first, because that erases for good.
    fn request(&mut self, id: MessageId, action: Action) {
        if matches!(action, Action::Reply | Action::ReplyAll | Action::Forward) {
            self.answer(id, action);
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

    /// Runs `action` on several messages at once, as [`Self::request`] does on one. Spam and Not spam
    /// mark the senders and keep the messages as examples, as one at a time.
    fn request_all(&mut self, ids: Vec<MessageId>, action: Action) {
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
            _ => ids.iter().for_each(|id| self.act(*id, action)),
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
            && let Some(open) = self.message.filter(|open| *open != id && self.listed().contains(open))
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
            self.show_listed(only);
        }
    }

    /// Shift+click: picks every message from the first picked (or the open one) to `id`, in list order.
    fn pick_range(&mut self, id: MessageId) {
        let ids = self.listed();
        let Some(anchor) = self.picked.first().copied().or(self.message) else { return self.show_listed(id) };
        if let Some(range) = range(&ids, anchor, id) {
            self.picked = range;
        }
    }

    /// Opens a reply or a forward of the open message.
    fn answer(&mut self, id: MessageId, action: Action) {
        let Some(serial) = self.open_tab().map(|t| t.serial) else { return };
        let (Some(summary), Some(open)) = (self.summary_of(id), &self.body) else { return };
        let body = &open.data;
        let Some(view) = self.view(serial).filter(|v| v.sender.is_some()) else { return };
        let own = view.account.address();
        let key = view.account.key();
        let mut compose = match action {
            Action::Forward => compose::Compose::forward(serial, &key, summary, body, &body.attachments),
            _ => compose::Compose::reply(serial, own, summary, body, matches!(action, Action::ReplyAll)),
        };
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
        if let Err(e) = drafts::delete(&self.vault, &compose.key, &compose.id) {
            self.error = Some(format!("Could not delete the draft: {e}"));
        }
        // A copy was put on the server: take it away too.
        if compose.may_be_on_server() {
            let source = self.view(compose.from).and_then(|v| v.source.clone());
            if let Some(source) = source {
                let id = compose.id.clone();
                let serial = compose.from;
                self.spawn(async move { Event::DraftPushed(serial, id.clone(), 0, source.delete_draft(&id).await) });
            }
        }
        self.refresh_drafts();
    }

    /// Stores a copy of the draft in the account's Drafts folder, so other devices see it. The files are
    /// read from the vault and the message is built off the window's thread.
    fn push_draft(&mut self, compose: &mut compose::Compose) {
        let Some(view) = self.view(compose.from) else { return };
        let Some(source) = view.source.clone() else { return };
        let message = compose.outgoing(view.account.address(), self.settings.rich_outgoing);
        let (files, vault, id, snapshot, serial) = (compose.file_list(), self.vault.clone(), compose.id.clone(), compose.snapshot(), compose.from);
        compose.pushing = true;
        self.spawn(async move {
            let built = tokio::task::spawn_blocking({
                let (id, mut message) = (id.clone(), message);
                move || -> Result<Vec<u8>, String> {
                    message.attachments = attachments::read_files(&vault, &files)?;
                    render_draft(&message, &id).map_err(|e| e.to_string())
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
        if self.form.is_some() || !self.confirm_erase.is_empty() || self.confirm_spam || self.confirm_empty.is_some() || self.compose.is_some() || self.show_settings || self.locked() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let key = |k| ctx.input(|i| i.key_pressed(k));
        if key(egui::Key::C) {
            self.new_message();
            return;
        }
        if key(egui::Key::Slash) {
            self.focus_search = true;
            return;
        }
        // In a message tab the list is not on screen: no moving through it.
        let ids: Vec<MessageId> = if self.in_tab { Vec::new() } else { self.listed() };
        let pos = self.message.and_then(|id| ids.iter().position(|i| *i == id));
        if key(egui::Key::Escape) {
            self.picked.clear();
        }
        if !self.in_tab && ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::A))) {
            self.picked = ids.clone();
            return;
        }
        let step = if key(egui::Key::ArrowDown) { Some(1) } else if key(egui::Key::ArrowUp) { Some(-1) } else { None };
        if let Some(step) = step {
            let next = pos.map_or(0, |p| p.saturating_add_signed(step)).min(ids.len().saturating_sub(1));
            if let Some(id) = ids.get(next).copied().filter(|id| Some(*id) != self.message) {
                self.show_listed(id);
            }
        }
        let shortcut = |modifiers, key| ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(modifiers, key)));
        if shortcut(egui::Modifiers::COMMAND, egui::Key::W) {
            if let Some(at) = self.active().filter(|_| self.in_tab) {
                self.close_tab(at);
            }
            return;
        }
        let back = shortcut(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Tab);
        if back || shortcut(egui::Modifiers::COMMAND, egui::Key::Tab) {
            self.cycle_tabs(back);
            return;
        }
        if self.picked.len() >= 2 {
            let ids = self.targets();
            let spam = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "!")));
            if key(egui::Key::E) {
                self.request_all(ids, Action::Archive);
            } else if key(egui::Key::Delete) {
                self.request_all(ids, Action::Trash);
            } else if key(egui::Key::U) {
                let unread = ids.iter().any(|id| self.summary_of(*id).is_some_and(|m| m.unread));
                self.request_all(ids, Action::Unread(!unread));
            } else if spam && self.picked_role() == Role::Inbox {
                self.request_all(ids, Action::Spam);
            }
            return;
        }
        let Some(id) = self.message else { return };
        if key(egui::Key::Enter) && pos.is_some() {
            self.open_message(id, true);
        } else if key(egui::Key::R) {
            self.request(id, Action::Reply);
        } else if key(egui::Key::A) {
            self.request(id, Action::ReplyAll);
        } else if key(egui::Key::F) {
            self.request(id, Action::Forward);
        } else if key(egui::Key::E) {
            self.request(id, Action::Archive);
        } else if key(egui::Key::Delete) {
            self.request(id, Action::Trash);
        } else if key(egui::Key::U) {
            let unread = self.summary_of(id).is_some_and(|m| m.unread);
            self.request(id, Action::Unread(!unread));
        } else if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "!"))) && self.role_of(id) == Role::Inbox {
            // "!" as typed, whatever the keyboard layout puts it on: spam.
            self.correct(id, Correction::Spam);
        }
    }

    /// Adds the account, or applies an edit. The password goes to the credential store first and
    /// the settings file second, so a failure never leaves one without the other.
    fn save_account(&mut self, form: &NewAccount) -> Result<(), String> {
        let account = form.build()?;
        let editing = form.editing.and_then(|serial| self.accounts.iter().position(|v| v.serial == serial));
        let old = editing.map(|at| self.accounts[at].account.clone());
        if self.accounts.iter().enumerate().any(|(i, v)| Some(i) != editing && v.account.key() == account.key()) {
            return Err("this account is already configured".into());
        }
        let password = match (&old, form.password.is_empty()) {
            (Some(old), true) => store::password(old)?, // moving the stored password to the new key
            _ => form.password.clone(),
        };
        store::set_password(&account, &password)?;
        let mut all: Vec<Account> = self.accounts.iter().map(|v| v.account.clone()).collect();
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
            self.add_view(account);
            return Ok(());
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
        if let Some(compose) = &mut self.compose
            && compose.from == old.serial
        {
            compose.from = serial;
        }
        Ok(())
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
    }

    fn apply(&mut self, event: Event) {
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
                        let inbox = view.mailboxes.iter().position(|m| m.role == Role::Inbox).unwrap_or(0);
                        let moved = selected_name.and_then(|name| view.mailboxes.iter().position(|m| m.name == name));
                        if self.selected.is_some_and(|(s, _)| s == serial) {
                            self.selected = moved.map(|i| (serial, i));
                        } else if self.selected.is_none() && !view.mailboxes.is_empty() {
                            self.select_mailbox(serial, inbox);
                        }
                    }
                    Err(e) => view.error = Some(e.to_string()),
                }
                if download {
                    self.start_download(serial);
                    self.start_watch(serial);
                }
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
                    Err(e) => self.error = Some(format!("Downloading mail stopped: {e}")),
                }
                // Previews arrive with the files.
                if let Some((s, index)) = self.selected.filter(|(s, _)| *s == serial)
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
                if self.selected != Some((serial, index)) {
                    return;
                }
                match result {
                    Ok(report) => {
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
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            Event::Messages(serial, index, result) if self.selected == Some((serial, index)) => match result {
                Ok(messages) => {
                    self.messages = messages;
                    self.error = None;
                    self.screen_inbox(serial, index);
                }
                Err(e) => self.error = Some(e.to_string()),
            },
            Event::Screened(serial, index, result) if self.selected == Some((serial, index)) => {
                self.checking = false;
                match result {
                    Ok(verdicts) => self.screening.extend(verdicts),
                    Err(e) => {
                        // Without verdicts the Inbox shows everything rather than hiding mail.
                        self.screening.clear();
                        self.error = Some(format!("The spam checks could not run, so the Inbox shows all mail: {e}"));
                    }
                }
            }
            Event::Body(serial, id, result) if self.message == Some(id) && self.open_tab().is_some_and(|t| t.serial == serial) =>
            {
                match result {
                    Ok(body) => {
                        let findings = self.findings(serial, id, &body);
                        self.body = Some(OpenBody::new(body, findings));
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            Event::Done(serial, index, Err(e)) => {
                self.error = Some(e.to_string());
                if self.selected == Some((serial, index)) {
                    self.select_mailbox(serial, index); // show what the server really has
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
            Event::Saved(result) => match result {
                Ok(Some(text)) => {
                    self.notice = Some((text, std::time::Instant::now()));
                    self.attach_selected.clear();
                }
                Ok(None) => {}
                Err(e) => self.error = Some(e),
            },
            Event::Sent(result, copy_error) => match result {
                Ok(()) => {
                    if let Some(sent) = self.compose.take() {
                        self.delete_draft(&sent);
                    }
                    self.notice = Some(("Message sent".into(), std::time::Instant::now()));
                    if let Some(e) = copy_error {
                        self.error = Some(format!("The message was sent, but its copy could not be saved to Sent: {e}"));
                    }
                }
                Err(e) => {
                    if let Some(compose) = &mut self.compose {
                        compose.sending = false;
                        compose.error = Some(e.to_string());
                    }
                }
            },
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
    fn empty_spam(&mut self, serial: u64, index: usize) {
        let Some(view) = self.view(serial) else { return };
        let (Some(source), Some(folder)) = (view.source.clone(), view.mailboxes.get(index).map(|m| m.name.clone())) else { return };
        let account = view.account.key();
        match self.archive.messages(&account, &folder) {
            Ok(messages) => self.keep_examples(&account, &messages, true),
            Err(e) => {
                self.error = Some(format!("Could not read the Spam folder: {e}"));
                return;
            }
        }
        if self.selected == Some((serial, index)) {
            self.messages.clear();
        }
        self.tabs.retain(|t| t.serial != serial || t.role != Role::Junk);
        if let Some(mailbox) = self.accounts.iter_mut().find(|v| v.serial == serial).and_then(|v| v.mailboxes.get_mut(index)) {
            mailbox.total = 0;
            mailbox.unread = 0;
        }
        self.spawn(async move { Event::Done(serial, index, source.empty(Role::Junk).await.map(drop)) });
    }

    /// Confirms emptying a Spam folder: nothing in it can be brought back afterwards.
    fn empty_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some((serial, index)) = self.confirm_empty else { return };
        let count = self.view(serial).and_then(|v| Some(self.archive.messages(&v.account.key(), &v.mailboxes.get(index)?.name).ok()?.len())).unwrap_or(0);
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_empty")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading(format!("Delete {} message{} in Spam?", views::thousands(count as u32), if count == 1 { "" } else { "s" }));
            ui.label(RichText::new("They will be erased from the server and cannot be recovered. The spam checks keep learning from them.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let erase = egui::Button::new(RichText::new("Empty Spam").color(t.on_accent)).fill(t.danger);
                confirm = ui.add_enabled(count > 0, erase).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.empty_spam(serial, index);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_empty = None;
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

    fn account_form(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else { return };
        let t = theme::Tokens::get(ctx);
        let (mut submit, mut cancel) = (false, false);
        let ready = form.ready();
        let editing = form.editing.is_some();
        let modal = egui::Modal::new(egui::Id::new("add_account")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading(if editing { "Edit account" } else { "Add account" });
            ui.label(RichText::new("Connect an existing mailbox over IMAP and SMTP.").color(t.text_dim));
            ui.add_space(8.0);
            form_grid(ui, "account_form", |ui| {
                ui.label("Name");
                let name = ui.add(field(&mut form.name).hint_text("Optional"));
                if std::mem::take(&mut form.focus_first) {
                    name.request_focus();
                }
                ui.end_row();
                ui.label("Email");
                ui.add(field(&mut form.email).hint_text("Address mail is sent from"));
                ui.end_row();
                ui.label("User");
                ui.add(field(&mut form.user).hint_text("Login name, often the same address"));
                ui.end_row();
                ui.label("Password");
                let hint = if editing { "Leave empty to keep the current one" } else { "" };
                ui.add(field(&mut form.password).password(true).hint_text(hint));
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Incoming mail (IMAP)").strong());
            form_grid(ui, "imap_form", |ui| {
                ui.label("Server");
                ui.add(field(&mut form.imap_host).hint_text("imap.example.com"));
                ui.end_row();
                ui.label("Port");
                ui.horizontal(|ui| {
                    ui.add(sized_field(&mut form.imap_port, 72.0));
                    tls_picker(ui, "imap_tls", &mut form.imap_tls);
                });
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Outgoing mail (SMTP)").strong());
            form_grid(ui, "smtp_form", |ui| {
                ui.label("Server");
                ui.add(field(&mut form.smtp_host).hint_text("Same as IMAP"));
                ui.end_row();
                ui.label("Port");
                ui.horizontal(|ui| {
                    ui.add(sized_field(&mut form.smtp_port, 72.0));
                    tls_picker(ui, "smtp_tls", &mut form.smtp_tls);
                });
                ui.end_row();
            });
            ui.horizontal(|ui| {
                ui.label("Keep mail offline");
                let label = |k: Keep| match k {
                    Keep::All => "Everything",
                    Keep::Year => "Last 12 months",
                    Keep::Month => "Last 30 days",
                    Keep::Opened => "Only messages I open",
                };
                egui::ComboBox::from_id_salt("keep").selected_text(label(form.keep)).show_ui(ui, |ui| {
                    for keep in [Keep::All, Keep::Year, Keep::Month, Keep::Opened] {
                        ui.selectable_value(&mut form.keep, keep, label(keep));
                    }
                });
            });
            ui.checkbox(&mut form.save_sent, "Keep a copy of sent messages in Sent")
                .on_hover_text("Turn off for providers that already do it, such as Gmail and Microsoft 365");
            if let Some(error) = &form.error {
                ui.add_space(8.0);
                ui.colored_label(t.danger, error);
            }
            ui.add_space(12.0);
            // A fixed-height row: a bare right-to-left layout would claim the rest of the window.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let add = egui::Button::new(RichText::new(if editing { "Save changes" } else { "Add account" }).color(t.on_accent)).fill(t.accent);
                let add = ui.add_enabled(ready, add).on_disabled_hover_text("User, password and IMAP server are required");
                submit = add.clicked() || (ready && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                cancel = ui.button("Cancel").clicked();
            });
        });
        let mut open = !(cancel || modal.should_close());
        if submit {
            match self.save_account(&form) {
                Ok(()) => open = false,
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
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::symmetric(14, 5));
        egui::Panel::bottom("status").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                if locked {
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
                });
            });
        });
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

    fn error_banner(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        const NOTICE_SECS: f32 = 4.0;
        if let Some((text, since)) = self.notice.clone() {
            let left = NOTICE_SECS - since.elapsed().as_secs_f32();
            if left <= 0.0 {
                self.notice = None;
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f32(left));
                let banner = egui::Frame::new().fill(t.accent.gamma_multiply(0.16)).inner_margin(egui::Margin::symmetric(16, 10));
                egui::Panel::top("notice").frame(banner).show(ui, |ui| {
                    ui.label(RichText::new(text).color(t.accent));
                });
            }
        }
        let Some(error) = self.error.clone() else { return };
        let banner = egui::Frame::new().fill(t.danger.gamma_multiply(0.14)).inner_margin(egui::Margin::symmetric(16, 10));
        egui::Panel::top("error").frame(banner).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(t.danger, error);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Dismiss").clicked() {
                        self.error = None;
                    }
                });
            });
        });
    }

    /// Accounts and their folders. Returns (folder chosen, account to remove, account to edit, add-account clicked).
    fn sidebar(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> SidebarOutcome {
        let (mut chosen, mut removed, mut edit, mut add, mut suspicious, mut recheck, mut empty) = (None, None, None, false, None, None, None);
        // shortcut: only the Inbox being shown has verdicts, so only its suspicious mail shows a count; check
        // every account in the background (during sync) to count them all.
        let held = self.held() as u32;
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::same(12));
        egui::Panel::left("accounts").frame(frame).default_size(260.0).size_range(210.0..=340.0).show(ui, |ui| {
            let can_send = !self.senders().is_empty();
            let why = if can_send { None } else { Some("Add an account to send mail") };
            if views::primary_wide(ui, t, Icon::Pencil, "New email", why).on_hover_text("New message (C)").clicked() && can_send {
                self.new_message();
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                    let width = ui.available_width() - theme::CONTROL_H;
                    add = ui.add_sized([width, theme::CONTROL_H], egui::Button::image_and_text(Icon::Plus.image(icons::SIZE, t.text_dim), "Add account")).clicked();
                    if icons::button(ui, t, Icon::Settings, "Settings", true, self.show_settings).clicked() {
                        self.show_settings = true;
                        self.security.mode = None;
                    }
                });
                ui.add_space(4.0);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.label(RichText::new("ACCOUNTS").size(12.0).color(t.text_dim));
                        for view in &self.accounts {
                            let header = egui::CollapsingHeader::new(RichText::new(&view.account.name).strong())
                                .id_salt(view.serial)
                                .default_open(true)
                                .show(ui, |ui| {
                                    if let Some(error) = &view.error {
                                        ui.colored_label(t.danger, error);
                                    }
                                    let mut order: Vec<usize> = (0..view.mailboxes.len()).collect();
                                    order.sort_by_key(|&i| (views::folder_rank(view.mailboxes[i].role), view.mailboxes[i].name.clone()));
                                    for i in order {
                                        let mailbox = &view.mailboxes[i];
                                        let title = views::folder_title(&mailbox.name, mailbox.role);
                                        let here = self.selected == Some((view.serial, i));
                                        let holding = self.settings.hold_suspicious && mailbox.role == Role::Inbox;
                                        let tip = format!("{} message{}  ·  {} unread", views::thousands(mailbox.total), if mailbox.total == 1 { "" } else { "s" }, views::thousands(mailbox.unread));
                                        let row = views::folder_row(ui, t, &title, mailbox.role, mailbox.unread, here && !(holding && self.show_suspicious)).on_hover_text(tip);
                                        if row.clicked() {
                                            chosen = Some((view.serial, i));
                                        }
                                        row.context_menu(|ui| folder_menu(ui, mailbox.role, self.settings.hold_suspicious, &mut recheck, &mut empty, (view.serial, i)));
                                        if holding {
                                            let count = if here { held } else { 0 };
                                            let row = views::suspicious_row(ui, t, count, here && self.show_suspicious).on_hover_text("Mail from new senders that failed a check, held apart from the Inbox");
                                            if row.clicked() {
                                                suspicious = Some((view.serial, i));
                                            }
                                            row.context_menu(|ui| folder_menu(ui, mailbox.role, true, &mut recheck, &mut empty, (view.serial, i)));
                                        }
                                    }
                                });
                            header.header_response.context_menu(|ui| {
                                if ui.button("Edit account…").clicked() {
                                    edit = Some(view.serial);
                                    ui.close();
                                }
                                if ui.button("Remove account").clicked() {
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
        SidebarOutcome { chosen, suspicious, recheck, empty, removed, edit, add }
    }

    /// Search, refresh and the message list. Returns the message the user clicked and whether to add an account.
    fn message_list(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> (Option<MessageId>, bool) {
        let (mut opened, mut add, mut refresh) = (None, false, false);
        let (mut open_draft, mut delete_draft): (Option<String>, Option<String>) = (None, None);
        let mut ask_spam = false;
        let (mut pinned, mut toggled, mut ranged): (Option<MessageId>, Option<MessageId>, Option<MessageId>) = (None, None, None);
        let mut clicked_hit: Option<(u64, Hit, bool)> = None;
        let frame = egui::Frame::new().fill(t.surface);
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
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
                    .on_hover_text("Search everything stored on this computer.\nWords, \"a phrase\", from:ada, subject:invoice, in:inbox, is:unread, has:attachment");
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
            if self.searching() {
                if self.hits.is_empty() {
                    views::empty_state(ui, t, "No results", "Nothing stored on this computer matches. Try fewer words.", None);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, views::MESSAGE_ROW_H, self.hits.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (serial, hit) in &self.hits[range] {
                        let id = hit.message.id;
                        let selected = if self.picked.len() >= 2 { self.picked.contains(&id) } else { self.message == Some(id) };
                        let row = views::message_row(ui, t, &hit.message, selected, Some(&hit.folder), None);
                        let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
                        let keep = row.double_clicked() || row.middle_clicked();
                        if row.clicked() && !keep && ctrl {
                            toggled = Some(id);
                        } else if row.clicked() && !keep && shift {
                            ranged = Some(id);
                        } else if keep || row.clicked() {
                            clicked_hit = Some((*serial, hit.clone(), keep));
                        }
                    }
                });
                return;
            }
            if !self.drafts.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new(format!("Drafts on this computer ({})", self.drafts.len())).size(13.0).strong().color(t.text_dim));
                });
                egui::ScrollArea::vertical().id_salt("drafts").max_height(views::MESSAGE_ROW_H * 2.6).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for draft in &self.drafts {
                        match views::draft_row(ui, t, draft) {
                            views::DraftAction::Open => open_draft = Some(draft.id.clone()),
                            views::DraftAction::Delete => delete_draft = Some(draft.id.clone()),
                            views::DraftAction::None => {}
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
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, views::MESSAGE_ROW_H, shown.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for message in &shown[range] {
                    let warning = self.correction(message.id).and_then(|(_, warning)| warning);
                    let selected = if self.picked.len() >= 2 { self.picked.contains(&message.id) } else { self.message == Some(message.id) };
                    let row = views::message_row(ui, t, message, selected, None, warning);
                    let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
                    if row.double_clicked() || row.middle_clicked() {
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
        });
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
            self.open_hit(serial, &hit, pin);
        }
        if let Some(id) = open_draft {
            self.open_draft(&id);
        }
        if let Some(id) = delete_draft
            && let Some((serial, _)) = self.selected
            && let Some(key) = self.view(serial).map(|v| v.account.key())
        {
            if let Err(e) = drafts::delete(&self.vault, &key, &id) {
                self.error = Some(format!("Could not delete the draft: {e}"));
            }
            // Its copy in the server's Drafts folder goes too.
            if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
                let gone = id.clone();
                self.spawn(async move { Event::DraftPushed(serial, gone.clone(), 0, source.delete_draft(&gone).await) });
            }
            self.refresh_drafts();
        }
        (opened, add)
    }

    /// The open message with its action bar. Returns the action the user chose.
    fn reader(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> ReaderOutcome {
        let mut action = None;
        let mut url = None;
        let mut chosen_mode = None;
        let mut remember = None;
        let mut attachment = None;
        let mut save_many = false;
        let mut allow_sender = None;
        let mut mark = None;
        let mut suspicious = false;
        // The correction that makes sense; the reasons are the warnings shown just above it.
        let correction = self.message.and_then(|id| self.correction(id)).map(|(choice, _)| choice);
        let (settings_mode, view_mode) = (self.settings.clone(), self.view_mode);
        let selected_files = self.attach_selected.clone();
        // The message in the reader is its tab's: it may come from another folder than the one shown.
        let open = self.open_tab().map(|t| (t.message.clone(), t.role, t.serial));
        let server_host = open.as_ref().and_then(|(_, _, serial)| self.view(*serial)).map(|v| v.account.imap.host.clone()).unwrap_or_default();
        let role = open.as_ref().map_or(Role::Other, |(_, role, _)| *role);
        let current = open.map(|(message, ..)| message);
        let frame = egui::Frame::new().fill(t.surface);
        let in_tab = self.in_tab;
        let picked = if in_tab { 0 } else { self.picked.len() };
        let picked_unread = self.picked.iter().any(|id| self.summary_of(*id).is_some_and(|m| m.unread));
        let folder_role = self.picked_role();
        let mut bulk = None;
        let draw = |ui: &mut egui::Ui| {
            if picked >= 2 {
                bulk = views::picked_panel(ui, t, picked, picked_unread, folder_role);
                return;
            }
            let Some(message) = current else {
                views::empty_state(ui, t, "No message selected", "Choose a message from the list to read it. Double-click to open it in a tab.", None);
                return;
            };
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let archived = role == Role::Archive;
                let loaded = self.body.is_some();
                let answers = [
                    (Icon::Reply, "Reply (R)", Action::Reply),
                    (Icon::ReplyAll, "Reply all (A)", Action::ReplyAll),
                    (Icon::Forward, "Forward (F)", Action::Forward),
                ];
                for (icon, tip, what) in answers {
                    if icons::button(ui, t, icon, tip, loaded, false).clicked() {
                        action = Some((message.id, what));
                    }
                }
                ui.add_space(8.0);
                let tools = [
                    (Icon::Archive, "Archive (E)", Action::Archive, !archived),
                    (
                        if message.unread { Icon::MailOpen } else { Icon::Mail },
                        if message.unread { "Mark as read (U)" } else { "Mark as unread (U)" },
                        Action::Unread(!message.unread),
                        true,
                    ),
                    (Icon::Trash, if role == Role::Trash { "Delete permanently (Delete)" } else { "Move to Trash (Delete)" }, Action::Trash, true),
                ];
                for (icon, tip, what, enabled) in tools {
                    let tip = if !enabled { "Already in the archive" } else { tip };
                    if icons::button(ui, t, icon, tip, enabled, false).clicked() {
                        action = Some((message.id, what));
                    }
                }
            });
            ui.add_space(6.0);
            ui.separator();
            match &mut self.body {
                None => {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
                Some(body) => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        // A margin, not `ui.horizontal`: horizontal layouts do not wrap text, so a long
                        // line would widen the whole panel.
                        egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 0)).show(ui, |ui| {
                            ui.set_max_width(ui.available_width().min(720.0));
                            ui.add_space(20.0);
                            ui.heading(&message.subject);
                            ui.add_space(12.0);
                            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), views::AVATAR), egui::Sense::hover());
                            views::avatar(ui, t, &message.from, rect.left_center() + vec2(views::AVATAR / 2.0, 0.0), views::AVATAR);
                            let name = ui.painter().layout_no_wrap(message.from.clone(), egui::FontId::proportional(15.5), t.text);
                            let detail = if message.address.is_empty() || message.address == message.from { message.received.clone() } else { format!("{}  ·  {}", message.address, message.received) };
                            let date = ui.painter().layout_no_wrap(detail, egui::FontId::proportional(13.0), t.text_dim);
                            ui.painter().galley(pos2(rect.left() + views::AVATAR + 12.0, rect.top() + 3.0), name, t.text);
                            ui.painter().galley(pos2(rect.left() + views::AVATAR + 12.0, rect.top() + 23.0), date, t.text_dim);
                            let authenticity = sender::authenticity(&body.data.auth, &server_host, &message.address);
                            for finding in &body.findings {
                                let color = if finding.level == screen::Level::Danger { t.danger } else { t.text_dim };
                                ui.add_space(6.0);
                                ui.horizontal_wrapped(|ui| {
                                    ui.add(Icon::Spam.image(icons::SIZE, color));
                                    ui.label(RichText::new(&finding.text).size(13.0).color(color));
                                });
                            }
                            if body.findings.is_empty() && authenticity == sender::Authenticity::Verified {
                                ui.add_space(6.0);
                                ui.label(RichText::new(format!("Sender domain verified by your server: {}", sender::domain(&message.address))).size(13.0).color(t.text_dim));
                            }
                            suspicious = body.findings.iter().any(|f| f.level == screen::Level::Danger);
                            if let Some(choice) = correction {
                                let key = screen::sender_key(&message.address);
                                let who = key.trim_start_matches('@');
                                let (text, held, button, tip) = match choice {
                                    Correction::NotSpam => ("Held apart from the Inbox as suspicious.".to_owned(), true, "Not spam", format!("Show mail from {who} in the Inbox from now on")),
                                    Correction::Spam => (format!("First mail from {who}."), false, "Spam", format!("Move mail from {who} to the Spam folder, now and from now on")),
                                    Correction::OutOfSpam => ("In the Spam folder.".to_owned(), true, "Not spam", format!("Move it back to the Inbox, and keep mail from {who} there from now on")),
                                };
                                ui.add_space(10.0);
                                if views::mark_bar(ui, t, &text, held, button, &tip) {
                                    mark = Some(choice);
                                }
                            }
                            if !body.data.to.is_empty() {
                                ui.add_space(6.0);
                                ui.add(egui::Label::new(RichText::new(format!("To: {}", body.data.to.join(", "))).size(13.0).color(t.text_dim)).truncate());
                            }
                            if !body.data.attachments.is_empty() {
                                ui.add_space(10.0);
                                let many = body.data.attachments.len() > 1;
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{} attachment{}", body.data.attachments.len(), if many { "s" } else { "" })).size(13.0).color(t.text_dim));
                                    if many {
                                        let label = if selected_files.is_empty() { "Save all…".to_owned() } else { format!("Save selected ({})…", selected_files.len()) };
                                        if ui.small_button(label).clicked() {
                                            save_many = true;
                                        }
                                    }
                                });
                                ui.add_space(2.0);
                                ui.horizontal_wrapped(|ui| {
                                    for file in &body.data.attachments {
                                        if let Some(what) = views::attachment_chip(ui, t, file, many.then(|| selected_files.contains(&file.index))) {
                                            attachment = Some((file.index, what));
                                        }
                                    }
                                });
                            }
                            let stored = settings_mode.mode_for(&message.address);
                            let mode = view_mode.unwrap_or(stored);
                            if body.simplified.is_some() {
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Show as").size(13.0).color(t.text_dim));
                                    for (label, value) in [("Text", DisplayMode::Plain), ("Simplified", DisplayMode::Simplified), ("Original", DisplayMode::Original)] {
                                        if ui.selectable_label(mode == value, label).clicked() {
                                            chosen_mode = Some(value);
                                        }
                                    }
                                    if mode != stored && !message.address.is_empty() && ui.small_button("Always for this sender").clicked() {
                                        remember = Some(mode);
                                    }
                                });
                            }
                            ui.add_space(16.0);
                            ui.separator();
                            ui.add_space(16.0);
                            if let (Some(source), DisplayMode::Original) = (&body.data.html, mode) {
                                let (width, scale) = (ui.available_width(), ui.ctx().pixels_per_point());
                                // A permission given to a sender never applies to a message that fails the server's checks.
                                let remote = body.remote || (settings_mode.allows_remote(&message.address) && !suspicious);
                                if !body.original.as_ref().is_some_and(|p| p.fits(width, scale, remote)) {
                                    body.original = original::Page::new(ui.ctx(), source, width, scale, remote);
                                }
                                if body.has_remote {
                                    match views::remote_bar(ui, t, remote, if suspicious { "" } else { &message.address }) {
                                        Some(views::Remote::Once) => body.remote = true,
                                        Some(views::Remote::Always) => allow_sender = Some(true),
                                        Some(views::Remote::Block) => {
                                            body.remote = false;
                                            allow_sender = Some(false);
                                        }
                                        None => {}
                                    }
                                    ui.add_space(8.0);
                                }
                                if let Some(page) = &mut body.original {
                                    url = page.show(ui);
                                }
                            } else {
                                let blocks = match (&body.simplified, mode) {
                                    (Some(simplified), DisplayMode::Simplified) => simplified,
                                    _ => &body.plain,
                                };
                                url = views::body(ui, t, blocks);
                            }
                            ui.add_space(28.0);
                        });
                    });
                }
            }
        };
        // A message tab has the window to itself; otherwise the reader is the pane on the right.
        if in_tab {
            egui::CentralPanel::default().frame(frame).show(ui, draw);
        } else {
            egui::Panel::right("reader").frame(frame).default_size(470.0).size_range(360.0..=900.0).show(ui, draw);
        }
        ReaderOutcome { action, url, mode: chosen_mode, remember, attachment, save_many, allow_sender, mark, bulk }
    }
}

impl eframe::App for BeaverApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = std::fs::remove_dir_all(&self.open_dir); // the decrypted copies made to open attachments
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }
        let t = theme::Tokens::get(ui.ctx());
        self.error_banner(ui, &t);
        self.status_bar(ui, &t);

        if self.in_tab && self.open_tab().is_none() {
            self.show_mail(); // its tab went with its folder or account
        }
        if self.tabs.iter().any(|t| t.pinned) {
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
        let SidebarOutcome { chosen, suspicious, recheck, empty, removed, edit, mut add } =
            if self.in_tab { SidebarOutcome::default() } else { self.sidebar(ui, &t) };
        if let Some(serial) = edit
            && let Some(view) = self.view(serial)
        {
            self.form = Some(NewAccount::editing(serial, &view.account));
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
        if empty.is_some() {
            self.confirm_empty = empty;
        }
        if let Some(serial) = removed {
            self.remove_account(serial);
        }
        self.account_form(ui.ctx());

        self.handle_keys(ui.ctx());
        let ReaderOutcome { action, url, mode, remember, attachment, save_many, allow_sender, mark, bulk } = self.reader(ui, &t);
        if let Some(action) = bulk {
            self.request_all(self.targets(), action);
        }
        if let (Some(choice), Some(id)) = (mark, self.message) {
            self.correct(id, choice);
        }
        if let Some((index, what)) = attachment {
            self.use_attachment(index, what);
        }
        if save_many {
            self.save_attachments(self.chosen_attachments());
        }
        if let Some(url) = url {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if mode.is_some() {
            self.view_mode = mode;
        }
        if let (Some(allow), Some(address)) = (allow_sender, self.current_address()) {
            self.settings.set_remote(&address, allow);
            self.save_settings();
        }
        if let (Some(mode), Some(address)) = (remember, self.current_address()) {
            self.settings.set_sender_mode(&address, mode);
            self.view_mode = None;
            self.save_settings();
        }
        let (opened, add_from_list) = if self.in_tab { (None, false) } else { self.message_list(ui, &t) };
        add |= add_from_list;

        if let Some(id) = opened {
            self.select_message(id);
        }
        if let Some((id, what)) = action {
            self.request(id, what);
        }
        self.erase_dialog(ui.ctx(), &t);
        self.spam_dialog(ui.ctx(), &t);
        self.empty_dialog(ui.ctx(), &t);
        self.open_dialog(ui.ctx(), &t);
        self.settings_window(ui.ctx(), &t);
        if self.locked()
            && let Some(request) = security::unlock_prompt(ui.ctx(), &t, &mut self.security, self.unlocking)
        {
            self.run_vault(request, ui.ctx());
        }
        if self.query != self.searched {
            self.run_search();
        }
        self.drop_files_on_the_app(ui.ctx(), &t);
        self.compose_window(ui.ctx(), &t);
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
fn folder_menu(ui: &mut egui::Ui, role: Role, holding: bool, recheck: &mut Option<(u64, usize)>, empty: &mut Option<(u64, usize)>, folder: (u64, usize)) {
    if role == Role::Junk {
        if ui.button("Empty Spam…").on_hover_text("Delete every message in Spam for good").clicked() {
            *empty = Some(folder);
            ui.close();
        }
        ui.separator();
    }
    let why = match (role, holding) {
        (Role::Inbox, true) => None,
        (Role::Inbox, false) => Some("Holding suspicious mail is turned off in Settings"),
        _ => Some("Only the Inbox is checked for spam"),
    };
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

/// The bytes of an attachment, through the mail source (a function so tasks can call it without a method).
async fn source_attachment(source: &ImapSource, id: MessageId, index: u32) -> Result<(String, Vec<u8>), MailError> {
    source.attachment(id, index).await
}

fn main() -> eframe::Result {
    if std::env::args().any(|a| a == "--screen-dry-run") {
        if let Err(e) = screen::dry_run() {
            eprintln!("beavermail: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("BeaverMail").with_inner_size([1280.0, 800.0]),
        // With vsync, Mesa's Wayland swap blocks the UI thread until the compositor sends a frame
        // callback, which it withholds for hidden windows; the compositor then reports the app as
        // not responding. egui only repaints on demand, so tearing is not a concern.
        glow_options: eframe::egui_glow::GlowConfiguration { vsync: false, ..Default::default() },
        ..Default::default()
    };
    eframe::run_native(
        "BeaverMail",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(BeaverApp::new(cc.egui_ctx.clone())))
        }),
    )
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
    fn a_range_runs_either_way_from_its_anchor() {
        let ids: Vec<MessageId> = (1..=5).map(MessageId).collect();
        assert_eq!(range(&ids, MessageId(2), MessageId(4)), Some(vec![MessageId(2), MessageId(3), MessageId(4)]));
        assert_eq!(range(&ids, MessageId(4), MessageId(2)), Some(vec![MessageId(4), MessageId(2), MessageId(3)]));
        assert_eq!(range(&ids, MessageId(3), MessageId(3)), Some(vec![MessageId(3)]));
        assert_eq!(range(&ids, MessageId(9), MessageId(3)), None, "an anchor no longer listed");
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
}
