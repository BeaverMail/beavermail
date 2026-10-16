// SPDX-License-Identifier: AGPL-3.0-only

//! BeaverMail desktop shell: accounts with their folders, a message list and a reader.
//! Accounts come from `accounts.toml` and the system credential store (see
//! `beavermail-accounts`). Mail calls run on a background runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

mod attachments;
mod commands;
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
mod threads;
mod unsubscribe;
mod views;

use beavermail_accounts as store;
use beavermail_core::{Account, DisplayMode, Hit, Keep, MailError, MailSender, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Progress, Role, Server, Settings, SyncReport, Tls};
use beavermail_imap::ImapSource;
use beavermail_smtp::{SmtpSender, render_draft};
use beavermail_store::Store;
use beavermail_vault::{Mode, SystemStore, Vault};
use eframe::egui::{self, Align, Layout, RichText, pos2, vec2};

use crate::commands::Command;
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
    /// The Sent folder of the account shown, for the user's replies in its conversations: (account, folder index shown, messages).
    SentList(u64, usize, Result<Vec<MessageSummary>, MailError>),
    Body(u64, MessageId, Result<Box<Prepared>, MailError>),
    /// The checks' verdicts on the Inbox loaded as (account, folder index).
    Screened(u64, usize, Result<std::collections::HashMap<MessageId, screen::Screened>, MailError>),
    /// A write (read flag, archive, trash) finished; the UI already showed its result.
    /// The third field is the message and action, for a change that can wait for the connection.
    Done(u64, usize, Option<(MessageId, Action)>, Result<(), MailError>),
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

/// How long an Archive or Move to Trash waits before going to the server, so Undo can take it back.
const UNDO_SECS: u64 = 10;

/// Archive or Move to Trash, shown at once but sent to the server only after [`UNDO_SECS`].
struct Pending {
    /// For the bar: "Archived 3 messages".
    text: String,
    until: std::time::Instant,
    /// Each message: (account, folder index shown, the action, the message as listed, its place in the list).
    items: Vec<(u64, usize, Action, MessageSummary, usize)>,
}

/// Runs `action` on message `id` on the server.
async fn perform(source: Arc<ImapSource>, id: MessageId, action: Action) -> Result<(), MailError> {
    match action {
        Action::Unread(unread) => source.set_unread(id, unread).await,
        Action::Archive => source.archive(id).await,
        Action::Trash => source.trash(id).await,
        Action::Spam => source.move_to(id, Role::Junk).await,
        Action::NotSpam => source.move_to(id, Role::Inbox).await,
        Action::Erase => source.erase(id).await,
        Action::Reply | Action::ReplyAll | Action::Forward => Ok(()),
    }
}

/// The command palette (Ctrl+K): what is typed and which line is chosen.
#[derive(Default)]
struct Palette {
    query: String,
    at: usize,
    /// The user moved with the arrows; until then the first line that can run is chosen.
    moved: bool,
}

/// A line of the palette: a command, or a folder to go to as (account, folder index).
#[derive(Clone)]
enum Entry {
    Run(Command),
    Folder(u64, usize),
}

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
    /// "Empty Spam" was chosen in this (account, Spam folder index)'s menu.
    empty: Option<(u64, usize)>,
    removed: Option<u64>,
    edit: Option<u64>,
    add: bool,
    /// "All inboxes" was clicked.
    all: bool,
    /// "Scheduled" was clicked.
    scheduled: bool,
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
    /// Images that only report the message was opened: never loaded (see [`html::trackers`]).
    trackers: Vec<String>,
    /// What deserves a second look about the message, the most serious first.
    findings: Vec<screen::Finding>,
}

impl OpenBody {
    fn new(p: Prepared) -> Self {
        Self { data: p.data, plain: p.plain, simplified: p.simplified, original: None, remote: false, has_remote: p.has_remote, trackers: p.trackers, findings: p.findings }
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
}

impl Prepared {
    fn new(data: MessageBody, findings: Vec<screen::Finding>) -> Self {
        let plain = html::plain_blocks(&data.text);
        let simplified = data.html.as_deref().map(html::blocks);
        let has_remote = data.html.as_deref().is_some_and(html::has_remote);
        let trackers = data.html.as_deref().map(html::trackers).unwrap_or_default();
        Self { data, plain, simplified, has_remote, trackers, findings }
    }
}

/// What the user did in the reader this frame.
#[derive(Default)]
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
    /// Unsubscribe was pressed: who sent the message, and how to leave the list.
    unsubscribe: Option<(String, unsubscribe::Method)>,
    /// Block sender was pressed on this message.
    block: Option<MessageId>,
    /// Another message of the conversation was chosen in the reader.
    switch: Option<MessageId>,
    /// "Show earlier" was pressed in the conversation.
    expand: bool,
    /// Back to the list, from a message shown in its place.
    back: bool,
    /// Trust was pressed for this message's sender.
    trust: Option<MessageId>,
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
    /// The server could not be reached the last time it was asked; the archive is shown.
    offline: bool,
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
    signature: String,
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
            signature: String::new(),
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
            signature: account.signature.clone(),
            focus_first: true,
            ..Self::default()
        }
    }

    /// Whether saving would change `account`: an invalid form counts as a change, so it is never dropped silently.
    fn changed(&self, account: &Account) -> bool {
        !self.password.is_empty() || self.build().map_or(true, |built| built != *account)
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
            signature: self.signature.trim_end().to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum SettingsPage {
    #[default]
    Reading,
    Writing,
    Spam,
    Storage,
    /// One account's page, by serial.
    Account(u64),
}

impl SettingsPage {
    const ALL: [Self; 4] = [Self::Reading, Self::Writing, Self::Spam, Self::Storage];

    fn title(self) -> &'static str {
        match self {
            Self::Reading => "Reading",
            Self::Writing => "Writing",
            Self::Spam => "Suspicious mail",
            Self::Storage => "Stored mail",
            Self::Account(_) => "Account",
        }
    }

    fn icon(self) -> Option<Icon> {
        match self {
            Self::Reading => Some(Icon::MailOpen),
            Self::Writing => Some(Icon::Pencil),
            Self::Spam => Some(Icon::Spam),
            Self::Storage => Some(Icon::Lock),
            Self::Account(_) => None,
        }
    }

    fn about(self) -> &'static str {
        match self {
            Self::Reading => "Most unwanted mail and tracking arrives as HTML, so messages start as text.",
            Self::Writing => "How messages you write are composed and sent.",
            Self::Spam => "Mail from people you know is never held. Nothing moves on the server.",
            Self::Storage => "How downloaded messages are kept on this computer.",
            Self::Account(_) => "",
        }
    }
}

const FORM_WIDTH: f32 = 460.0;
const SETTINGS_WIDTH: f32 = 780.0;
const PALETTE_WIDTH: f32 = 560.0;
/// How far the toast sits above the window's bottom edge: clear of the status bar.
const TOAST_LIFT: f32 = 44.0;
const PALETTE_LIST_H: f32 = 360.0;
/// The settings sidebar, page list and accounts.
const SETTINGS_NAV: f32 = 220.0;
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
fn pane_ring(ui: &egui::Ui, t: &theme::Tokens, rect: egui::Rect) {
    ui.painter().rect_stroke(rect.shrink(1.0), egui::CornerRadius::ZERO, egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(24.0);
    ui.label(RichText::new(text).size(theme::SUBTITLE).family(theme::bold()));
    ui.add_space(2.0);
}

/// One account's page in Settings; returns whether Remove account was pressed.
fn account_page(ui: &mut egui::Ui, t: &theme::Tokens, form: &mut NewAccount) -> bool {
    let hint = |ui: &mut egui::Ui, text: &str| ui.label(RichText::new(text).size(13.0).color(t.text_dim));
    form_grid(ui, "account_identity", |ui| {
        ui.label("Name");
        ui.add(field(&mut form.name).hint_text("Shown in the sidebar"));
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
    hint(ui, "Saving a change here reconnects the account. A different user or server downloads its mail again.");
    ui.add_space(4.0);
    form_grid(ui, "account_login", |ui| login_rows(ui, form));
    server_sections(ui, form);

    section_title(ui, "Remove account");
    hint(ui, "Deletes the mail downloaded to this computer and the saved password. Nothing changes on the server.");
    ui.add_space(4.0);
    let remove = egui::Button::new(RichText::new("Remove account…").color(t.danger)).stroke(egui::Stroke::new(1.0, t.danger));
    let pressed = ui.add(remove).clicked();
    ui.add_space(8.0);
    pressed
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
fn server_sections(ui: &mut egui::Ui, form: &mut NewAccount) {
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
    /// Where the user asked to go from an account page with unsaved changes (`None`: close Settings),
    /// until they choose to discard them.
    settings_leave: Option<Option<SettingsPage>>,
    /// The account the user asked to remove, until they confirm.
    confirm_remove: Option<u64>,
    /// Unsubscribe was pressed (who, how), until the user confirms.
    confirm_unsubscribe: Option<(String, unsubscribe::Method)>,
    /// Block sender was pressed on this message, until the user confirms.
    confirm_block: Option<MessageId>,
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
    /// Without the reading pane, the open message is shown in the list's place.
    full_preview: bool,
    /// The user chose to work offline: nothing goes to the network until they go online again. Not
    /// remembered, so a restart is online and the app is never offline without the user knowing.
    work_offline: bool,
    /// Changes made while the server could not be reached, made when it answers: (account, folder
    /// index, message, action). shortcut: kept in memory only, so closing the app drops them; keep
    /// them in the vault, as the Outbox does, if that turns out to matter.
    offline_changes: Vec<(u64, usize, MessageId, Action)>,
    /// The pane F6 moved the keys to; `None` until then, and again after a click in the list.
    pane: Option<Pane>,
    /// A scroll of the reader asked for from the keyboard, done when it is next drawn.
    reader_scroll: Option<Scroll>,
    /// The Inbox of every account is shown together; `selected` is then `None`.
    unified: bool,
    /// The account each listed message (and each Sent one) comes from.
    origin: std::collections::HashMap<MessageId, u64>,
    /// The Sent folder of the account shown, or of every account (empty when Sent itself is shown).
    sent: Vec<MessageSummary>,
    /// The conversations of the folder shown, worked out again when it changes.
    conversations: threads::Conversations,
    /// The reader shows every message of the open conversation, not only the latest few.
    whole_conversation: bool,
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
    /// The compose field last typed in: Esc takes the focus away, and Keep editing gives it back.
    compose_focus: Option<egui::Id>,
    refocus: u8,
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
            quitting: false,
            notice: None,
            settings: Settings::default(),
            archive: Arc::new(archive),
            vault: Arc::new(vault),
            syncing: 0,
            show_settings: false,
            settings_page: SettingsPage::default(),
            account_edit: None,
            settings_leave: None,
            confirm_remove: None,
            confirm_unsubscribe: None,
            confirm_block: None,
            palette: None,
            pending: None,
            outbox_sending: std::collections::HashMap::new(),
            outbox: 0,
            scheduled: Vec::new(),
            show_scheduled: false,
            offline_changes: Vec::new(),
            work_offline: false,
            full_preview: false,
            pane: None,
            reader_scroll: None,
            list_reveal: None,
            wanted: Arc::default(),
            unified: false,
            origin: std::collections::HashMap::new(),
            sent: Vec::new(),
            conversations: threads::Conversations::default(),
            whole_conversation: false,
            list_view: (0.0, 0.0),
            confirm_forget_marks: false,
            trust_input: String::new(),
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
            Ok(accounts) => accounts.into_iter().for_each(|a| {
                app.add_view(a);
            }),
            Err(e) => app.error = Some(e),
        }
        app.count_outbox(); // what waited from the last session shows at once
        app
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
                let sender = Arc::new(SmtpSender::new(&account, password.clone()));
                (Some(Arc::new(ImapSource::new(&account, password, self.archive.clone(), self.vault.clone()))), Some(sender), None)
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
        let view = AccountView { serial, account, source, sender, mailboxes: Vec::new(), error, offline: false, progress, watch_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)), watching: false };
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
        self.send_pending(); // Undo puts mail back in the list it left
        self.selected = Some((serial, index));
        self.unified = false;
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
        self.send_pending();
        self.selected = None;
        self.unified = true;
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
        (self.in_tab, self.full_preview) = (false, false);
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
        self.accounts.iter().filter_map(|v| Some((v.serial, v.mailboxes.iter().position(|m| m.role == Role::Inbox)?))).collect()
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
        let listed = self.messages.iter().find(|m| m.id == id).and_then(|m| Some(tabs::Tab { serial: self.serial_of(id)?, message: m.clone(), role: self.role(), pinned: pin }));
        let tabbed = || self.tabs.iter().find(|t| t.message.id == id).map(|t| tabs::Tab { serial: t.serial, message: t.message.clone(), role: t.role, pinned: pin });
        if let Some(tab) = listed.or_else(tabbed) {
            self.open(tab);
        }
    }

    /// Shows `tab`'s message, from any folder: in the reading pane, or when pinned, in its tab.
    fn open(&mut self, tab: tabs::Tab) {
        self.picked.clear();
        let (id, unread, pinned) = (tab.message.id, tab.message.unread, tab.pinned);
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
        // Without the reading pane, a message opens in the list's place.
        self.full_preview = !pinned && !self.settings.reading_pane;
        // Reading a conversation reads all of it, as its row shows it unread while any message is.
        let unread_here: Vec<MessageId> = self.conversation(id).into_iter().filter(|m| *m != id && self.summary_of(*m).is_some_and(|s| s.unread)).collect();
        if !judging && unread {
            self.act(id, Action::Unread(false));
        }
        if !judging {
            unread_here.into_iter().for_each(|m| self.act(m, Action::Unread(false)));
        }
    }

    /// Shows tab `at` in the reader. Its message is loaded again: from this computer when stored, so
    /// switching is quick without keeping every open message in memory.
    fn activate(&mut self, at: usize) {
        let Some(tab) = self.tabs.get(at) else { return };
        let (serial, id) = (tab.serial, tab.message.id);
        let Some(view) = self.view(serial) else { return };
        let Some(source) = view.source.clone() else { return };
        let (account, message) = (view.account.clone(), tab.message.clone());
        let own: Vec<String> = self.accounts.iter().map(|v| v.account.address().to_owned()).collect();
        let (archive, settings, wanted) = (self.archive.clone(), self.settings.clone(), self.wanted.clone());
        self.in_tab = tab.pinned;
        self.whole_conversation &= self.message.is_some_and(|m| self.conversations.same(m, id));
        self.message = Some(id);
        self.body = None;
        self.view_mode = None;
        self.attach_selected.clear();
        wanted.store(id.0, std::sync::atomic::Ordering::Relaxed);
        self.spawn(async move {
            // Paging through the list asks for many messages in a row: only the one it stops on is read.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            if wanted.load(std::sync::atomic::Ordering::Relaxed) != id.0 {
                return Event::Body(serial, id, Err(MailError::NotFound)); // dropped: another message is shown now
            }
            let body = match source.body(id).await {
                Ok(body) => body,
                Err(e) => return Event::Body(serial, id, Err(e)),
            };
            // The checks load what the account knows and train the spam model: seconds on a big mailbox, never on the UI thread.
            let prepared = tokio::task::spawn_blocking(move || {
                let findings = screen::Known::load(&archive, &account, &own, &settings)
                    .map(|known| screen::findings(&known, &message.from, &message.address, &body, &account.imap.host))
                    .unwrap_or_default();
                Box::new(Prepared::new(body, findings))
            });
            Event::Body(serial, id, prepared.await.map_err(|e| MailError::Backend(e.to_string())))
        });
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
    /// selected, unless a message is already shown in the list's place.
    fn move_to(&mut self, id: MessageId) {
        if self.settings.reading_pane || self.full_preview {
            self.show_listed(id);
        } else {
            self.message = Some(id);
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
        // Archive and Move to Trash of a listed message wait, so Undo can put it back.
        let undoable = matches!(action, Action::Archive | Action::Trash) && self.role_of(id) != Role::Trash;
        let kept = pos.filter(|_| undoable).map(|p| (p, self.messages[p].clone()));
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
        match kept {
            Some((pos, message)) => {
                let pending = self.pending.get_or_insert_with(|| Pending { text: String::new(), until: std::time::Instant::now(), items: Vec::new() });
                pending.items.push((serial, index, action, message, pos));
                pending.until = std::time::Instant::now() + std::time::Duration::from_secs(UNDO_SECS);
                let n = pending.items.len();
                let done = if matches!(action, Action::Archive) { "Archived" } else { "Moved to Trash" };
                pending.text = format!("{done} {n} message{}", if n == 1 { "" } else { "s" });
            }
            None => self.spawn(async move { Event::Done(serial, index, Some((id, action)), perform(source, id, action).await) }),
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
                view.offline = false;
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
    fn went_offline(&mut self, serial: u64) {
        if let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) {
            view.offline = true;
        }
    }

    /// Sends the Archive or Move to Trash that was waiting for Undo.
    fn send_pending(&mut self) {
        let Some(pending) = self.pending.take() else { return };
        for (serial, index, action, message, _) in pending.items {
            if let Some(source) = self.view(serial).and_then(|v| v.source.clone()) {
                self.spawn(async move { Event::Done(serial, index, Some((message.id, action)), perform(source, message.id, action).await) });
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
            self.select_message(id);
        }
    }

    /// "1,240 messages · 3 unread · 980 stored offline" for the folder being shown.
    fn folder_summary(&self) -> Option<String> {
        if self.unified && !self.searching() {
            let inboxes: Vec<&Mailbox> = self.shown().iter().filter_map(|(s, i)| self.view(*s)?.mailboxes.get(*i)).collect();
            let total = inboxes.iter().map(|m| m.total).sum::<u32>().max(self.messages.len() as u32);
            let unread: u32 = inboxes.iter().map(|m| m.unread).sum();
            let held = self.held();
            let mut parts = vec!["All inboxes".to_owned(), format!("{} message{}", views::thousands(total), if total == 1 { "" } else { "s" })];
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
        self.messages
            .iter()
            .filter(|m| !ours.contains(&m.message_id))
            .filter(|m| if checked(m) { self.screening.get(&m.id).is_some_and(|s| s.place == want) } else { !self.show_suspicious })
            // Newest first, so a conversation's row is its latest message here.
            .filter(|m| !threaded || self.conversations.number(m.id).is_none_or(|n| shown.insert(n)))
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
        if self.compose.is_some() {
            return;
        }
        let senders = self.senders();
        // The account of the folder shown, or in All inboxes the open message's.
        let shown = self.selected.map(|(s, _)| s).or_else(|| self.message.and_then(|id| self.serial_of(id)));
        let from = shown.filter(|s| senders.iter().any(|(x, _)| x == s)).or_else(|| senders.first().map(|(s, _)| *s));
        let signature = from.and_then(|f| self.view(f)).map(|v| v.account.signature.clone()).unwrap_or_default();
        self.start_compose(from.map(|f| compose::Compose::new(f).with_signature(&signature)));
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

    /// Hands the message to the server in the background; the answer comes back as `Event::Sent`.
    fn dispatch_send(&mut self, compose: &mut compose::Compose) {
        if self.send_message(compose) {
            compose.sending = true;
            compose.error = None;
        }
    }

    /// Sends `compose`'s message from its account in the background; false when the account cannot send.
    fn send_message(&self, compose: &compose::Compose) -> bool {
        let Some(view) = self.view(compose.from) else { return false };
        let (account, sender, source) = (view.account.clone(), view.sender.clone(), view.source.clone());
        let Some(sender) = sender else { return false };
        let mut message = compose.outgoing(account.address(), self.settings.rich_outgoing);
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
            for (serial, index, id, action) in now {
                let source = source.clone();
                self.spawn(async move { Event::Done(serial, index, Some((id, action)), perform(source, id, action).await) });
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
        let esc = !self.confirm_discard && !compose.sending && !compose.picking && !egui::Popup::is_any_open(ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
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
        let (mut save, mut discard, mut add, mut remove) = (false, false, false, false);
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
                            let row = views::settings_row(ui, t, &view.account.name, None, self.settings_page == page).on_hover_text(view.account.address());
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
                                views::avatar(ui, t, &account.name, rect.center(), views::AVATAR);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(RichText::new(&account.name).heading());
                                    ui.label(RichText::new(account.address()).color(t.text_dim));
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
                        let hint = |ui: &mut egui::Ui, text: &str| ui.label(RichText::new(text).size(13.0).color(t.text_dim));
                        match self.settings_page {
                            SettingsPage::Account(_) => {
                                if let Some(form) = &mut self.account_edit {
                                    remove = account_page(ui, t, form);
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
                                let about = "Off: the list takes the whole width and a message opens in its place; Esc or the arrow goes back.";
                                changed |= views::switch_row(ui, t, &mut self.settings.reading_pane, "Show the message beside the list", about).changed();
                                ui.add_space(12.0);
            if !self.settings.remote_senders.is_empty() {
                ui.label(RichText::new("Senders that may load remote content").family(theme::bold()));
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
            if !self.settings.sender_display.is_empty() {
                ui.label(RichText::new("Senders with their own setting").family(theme::bold()));
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
                                for (title, trusted, empty) in [
                                    ("", true, "None yet. Choose Trust on a message from a new sender, or add one here."),
                                    ("Marked as spam", false, "None. Spam or Block sender on a message puts its sender here."),
                                ] {
                                    if !title.is_empty() {
                                        section_title(ui, title);
                                    }
                                    let keys: Vec<String> = self.settings.marked.iter().filter(|(_, v)| **v == trusted).map(|(k, _)| k.clone()).collect();
                                    if keys.is_empty() {
                                        hint(ui, empty);
                                    }
                                    for key in keys {
                                        ui.horizontal(|ui| {
                                            let shown = match key.strip_prefix('@') {
                                                Some(domain) => format!("Everyone at {domain}"),
                                                None => key.clone(),
                                            };
                                            ui.label(shown);
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                if ui.small_button("Remove").clicked() {
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
                            SettingsPage::Storage => {
                                vault_request = security::section(ui, t, &mut self.security, mode, locked, job.as_deref());
                                if locked && ui.button("Unlock…").clicked() {
                                    unlock_again = true;
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
        if remove && let SettingsPage::Account(serial) = self.settings_page {
            self.confirm_remove = Some(serial);
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
        match target {
            Some(page) => self.settings_page = page,
            None => self.show_settings = false,
        }
    }

    /// Asks before an account page with unsaved changes is left.
    fn leave_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(target) = self.settings_leave else { return };
        let name = self.account_edit.as_ref().and_then(|f| self.view(f.editing?)).map(|v| v.account.name.clone()).unwrap_or_default();
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
        let mut entries: Vec<(Entry, String, String)> = Command::ALL.into_iter().filter(|c| *c != Command::Palette).map(|c| (Entry::Run(c), c.label().to_owned(), c.keys(ctx))).collect();
        for view in &self.accounts {
            let mut order: Vec<usize> = (0..view.mailboxes.len()).collect();
            order.sort_by_key(|&i| (views::folder_rank(view.mailboxes[i].role), view.mailboxes[i].name.clone()));
            for i in order {
                let mailbox = &view.mailboxes[i];
                entries.push((Entry::Folder(view.serial, i), format!("Go to {} › {}", view.account.name, views::folder_title(&mailbox.name, mailbox.role)), String::new()));
            }
        }
        entries.retain(|(_, label, _)| Command::matches(label, &palette.query));
        let why = |entry: &Entry| match entry {
            Entry::Run(c) => self.can(*c).err(),
            Entry::Folder(..) => None,
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
                views::search_box(ui, t, &mut palette.query, "Type a command or a folder", PALETTE_WIDTH, true);
            });
            if palette.query != before {
                palette.moved = false;
            }
            ui.add_space(8.0);
            if entries.is_empty() {
                ui.label(RichText::new("No command or folder matches.").color(t.text_dim));
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
            ui.label(RichText::new("↑ ↓ and Page Up/Down to choose  ·  Enter to run  ·  Esc to close").size(12.5).color(t.text_dim));
        });
        if let Some((entry, ..)) = chosen.and_then(|at| entries.get(at)) {
            match *entry {
                Entry::Run(command) => self.run(command),
                Entry::Folder(serial, index) => {
                    self.show_suspicious = false;
                    self.select_mailbox(serial, index);
                }
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

    /// Confirms removing an account: what was downloaded and the saved password go with it.
    fn remove_dialog(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(serial) = self.confirm_remove else { return };
        let Some(name) = self.view(serial).map(|v| v.account.name.clone()) else {
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
            return Role::Inbox;
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
            self.request_all(whole, action);
            if action.removes() && open && self.confirm_erase.is_empty()
                && let Some(next) = row.and_then(|r| self.listed().get(r).copied())
            {
                self.show_listed(next);
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
        if self.form.is_some() || !self.confirm_erase.is_empty() || self.confirm_spam || self.confirm_empty.is_some() || self.confirm_remove.is_some() || self.confirm_unsubscribe.is_some() || self.confirm_block.is_some() || self.compose.is_some() || self.show_settings || self.palette.is_some() || self.locked() {
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
        if typing {
            return;
        }
        let key = |k| ctx.input(|i| i.key_pressed(k));
        // In a message tab the list is not on screen: no moving through it.
        let ids: Vec<MessageId> = if self.in_tab { Vec::new() } else { self.listed() };
        let pos = self.message.and_then(|id| ids.iter().position(|i| self.conversations.same(*i, id) && (self.threaded() || *i == id)));
        let reading = self.in_tab || self.full_preview || self.pane == Some(Pane::Reader);
        if key(egui::Key::Escape) {
            self.picked.clear();
            if self.full_preview {
                self.full_preview = false; // back to the list
                return;
            }
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
            let next = match () {
                _ if key(egui::Key::ArrowDown) => Some(pos.map_or(0, |p| p.saturating_add(1))),
                _ if key(egui::Key::ArrowUp) => Some(pos.map_or(0, |p| p.saturating_sub(1))),
                _ if key(egui::Key::PageDown) => Some(pos.map_or(0, |p| p.saturating_add_signed(page))),
                _ if key(egui::Key::PageUp) => Some(pos.map_or(0, |p| p.saturating_add_signed(-page))),
                _ if key(egui::Key::Home) => Some(0),
                _ if key(egui::Key::End) => Some(last),
                _ => None,
            };
            if let Some(next) = next.map(|n| n.min(last)) {
                self.list_reveal = Some(next);
                if pos != Some(next) {
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
        match command {
            Command::Palette | Command::Search | Command::Settings => Ok(()),
            Command::NewMessage if self.senders().is_empty() => Err("Add an account to send mail"),
            Command::NewMessage => Ok(()),
            Command::Reply | Command::ReplyAll | Command::Forward => one().and(if self.body.is_some() { Ok(()) } else { Err("The message is still loading") }),
            Command::Archive | Command::Trash | Command::ToggleRead => some(),
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
                    self.request(id, answer(command));
                }
            }
            Command::Archive => self.request_all(targets, Action::Archive),
            Command::Trash => self.request_all(targets, Action::Trash),
            Command::Undo => self.undo(),
            Command::ToggleRead => {
                let unread = targets.iter().any(|id| self.summary_of(*id).is_some_and(|m| m.unread));
                self.request_all(targets, Action::Unread(!unread));
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
            Command::OpenInTab => {
                // Without the reading pane, Enter on a selected message opens it in the list's place.
                if let Some(id) = self.message {
                    if self.settings.reading_pane || self.full_preview { self.open_message(id, true) } else { self.show_listed(id) }
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
            && old.as_ref().is_some_and(|o| o.key() == account.key() && o.imap == account.imap && o.smtp == account.smtp)
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
        store::set_password(&account, &password)?;
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
            self.show_all_inboxes(); // its mail under the new connection
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
            self.show_all_inboxes(); // without the account's mail
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
                        view.offline &= !fresh; // the server answered
                        let inbox = view.mailboxes.iter().position(|m| m.role == Role::Inbox).unwrap_or(0);
                        let moved = selected_name.and_then(|name| view.mailboxes.iter().position(|m| m.name == name));
                        let has_inbox = view.mailboxes.iter().any(|m| m.role == Role::Inbox);
                        if self.selected.is_some_and(|(s, _)| s == serial) {
                            self.selected = moved.map(|i| (serial, i));
                        } else if self.unified {
                            // An account whose folders arrive after All inboxes was opened joins it now.
                            if has_inbox && !self.origin.values().any(|s| *s == serial) {
                                self.load_folder(serial, inbox);
                            }
                        } else if self.selected.is_none() && !view.mailboxes.is_empty() {
                            if self.accounts.len() >= 2 { self.show_all_inboxes() } else { self.select_mailbox(serial, inbox) }
                        }
                    }
                    Err(MailError::Offline(_)) => view.offline = true,
                    Err(e) => view.error = Some(e.to_string()),
                }
                if download {
                    self.start_download(serial);
                    self.start_watch(serial);
                    self.send_outbox(serial);
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
                    Err(MailError::Offline(_)) => self.went_offline(serial),
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
                            view.offline = false; // the server answered
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
                    Err(MailError::Offline(_)) => self.went_offline(serial),
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
                    Ok(prepared) => self.body = Some(OpenBody::new(*prepared)),
                    Err(MailError::Offline(_)) => {
                        self.went_offline(serial);
                        self.error = Some("This message is not stored on this computer. It opens once the server can be reached.".into());
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            // No connection: the change waits and is made when the server answers again.
            Event::Done(serial, index, Some(change), Err(MailError::Offline(_))) => {
                self.went_offline(serial);
                self.offline_changes.push((serial, index, change.0, change.1));
            }
            Event::Done(serial, index, _, Err(e)) => {
                self.error = Some(e.to_string());
                // Show what the server really has.
                if self.unified && self.shows(serial, index) {
                    self.show_all_inboxes();
                } else if self.selected == Some((serial, index)) {
                    self.select_mailbox(serial, index);
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
        self.spawn(async move { Event::Done(serial, index, None, source.empty(Role::Junk).await.map(drop)) });
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

    /// The Add account dialog; an existing account is changed on its page in Settings.
    fn account_form(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else { return };
        let t = theme::Tokens::get(ctx);
        let (mut submit, mut cancel) = (false, false);
        let ready = form.ready();
        let modal = egui::Modal::new(egui::Id::new("add_account")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading("Add account");
            ui.label(RichText::new("Connect an existing mailbox over IMAP and SMTP.").color(t.text_dim));
            ui.add_space(8.0);
            // Scrolls on a short window, so the buttons below stay in reach.
            egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 240.0).max(160.0)).show(ui, |ui| {
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
                    login_rows(ui, &mut form);
                });
                server_sections(ui, &mut form);
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
                } else if self.accounts.iter().any(|v| v.offline) {
                    let names: Vec<&str> = self.accounts.iter().filter(|v| v.offline).map(|v| v.account.name.as_str()).collect();
                    let who = if names.len() == self.accounts.len() { String::new() } else { format!(" ({})", names.join(", ")) };
                    let mut waiting = String::new();
                    if self.outbox > 0 {
                        waiting += &format!("  ·  {} waiting to be sent", views::thousands(self.outbox as u32));
                    }
                    if !self.offline_changes.is_empty() {
                        let n = self.offline_changes.len();
                        waiting += &format!("  ·  {n} change{} waiting", if n == 1 { "" } else { "s" });
                    }
                    ui.label(dim(format!("Offline{who}: showing mail stored on this computer{waiting}")));
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
        let (mut all, mut scheduled) = (false, false);
        let mut fold = None;
        // shortcut: only the Inboxes being shown have verdicts, so only their suspicious mail shows a count;
        // check every account in the background (during sync) to count them all.
        let mut held_of: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        for (id, _) in self.screening.iter().filter(|(_, s)| s.place == screen::Place::Spam) {
            if let Some(serial) = self.origin.get(id) {
                *held_of.entry(*serial).or_default() += 1;
            }
        }
        let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::same(12));
        egui::Panel::left("accounts").frame(frame).default_size(260.0).size_range(210.0..=340.0).show(ui, |ui| {
            let can_send = !self.senders().is_empty();
            let why = if can_send { None } else { Some("Add an account to send mail") };
            if views::primary_wide(ui, t, Icon::Pencil, "New email", why).on_hover_text(Command::NewMessage.tip(ui.ctx())).clicked() && can_send {
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
                        if self.accounts.len() >= 2 {
                            let unread = self.accounts.iter().filter_map(|v| v.mailboxes.iter().find(|m| m.role == Role::Inbox)).map(|m| m.unread).sum();
                            all = views::folder_row(ui, t, "All inboxes", Role::Inbox, unread, self.unified).on_hover_text("The Inbox of every account, newest first").clicked();
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
                            title.append(&view.account.name, 0.0, egui::TextFormat::simple(egui::FontId::proportional(15.0), t.text));
                            let header = egui::CollapsingHeader::new(title)
                                .id_salt(view.serial)
                                .default_open(!self.settings.folded.contains(&view.account.key()))
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
                                            let count = held_of.get(&view.serial).copied().unwrap_or(0);
                                            let row = views::suspicious_row(ui, t, count, here && self.show_suspicious).on_hover_text("Mail from new senders that failed a check, held apart from the Inbox");
                                            if row.clicked() {
                                                suspicious = Some((view.serial, i));
                                            }
                                            row.context_menu(|ui| folder_menu(ui, mailbox.role, true, &mut recheck, &mut empty, (view.serial, i)));
                                        }
                                    }
                                });
                            if header.header_response.clicked() {
                                fold = Some(view.account.key());
                            }
                            header.header_response.context_menu(|ui| {
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
        SidebarOutcome { chosen, suspicious, recheck, empty, removed, edit, add, all, scheduled }
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
                    .on_hover_text(format!("Search everything stored on this computer.\nWords, \"a phrase\", from:ada, to:bob, subject:invoice, in:inbox, is:unread, has:attachment, after:2026-09-01, before:2026-10\n\nAll commands and their keys: {}", Command::Palette.keys(ui.ctx())));
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
            let shown_rows = area.show_rows(ui, views::MESSAGE_ROW_H, shown.len(), |ui, range| {
                for message in &shown[range] {
                    let warning = self.correction(message.id).and_then(|(_, warning)| warning);
                    // A conversation's row: its latest message here, bold while any of it is unread.
                    let (count, unread) = if threaded { (self.conversations.members(message.id).len(), self.conversations.unread(message.id)) } else { (1, message.unread) };
                    let open = self.message.is_some_and(|id| if threaded { self.conversations.same(id, message.id) } else { id == message.id });
                    let selected = if self.picked.len() >= 2 { self.picked.contains(&message.id) } else { open };
                    let summary = MessageSummary { unread, ..(*message).clone() };
                    let mark = self.origin.get(&message.id).and_then(|s| self.accounts.iter().position(|v| v.serial == *s)).filter(|_| self.unified).map(theme::account_color);
                    let row = views::message_row(ui, t, &summary, selected, None, warning, count, mark);
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
            self.open_hit(serial, &hit, pin);
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
        let mut action = None;
        let mut url = None;
        let mut chosen_mode = None;
        let mut remember = None;
        let mut attachment = None;
        let mut save_many = false;
        let mut allow_sender = None;
        let mut mark = None;
        let mut suspicious = false;
        let (mut leave, mut block, mut trust) = (None, None, None);
        let scroll = self.reader_scroll.take();
        let (mut switch, mut expand) = (None, false);
        // The open message's conversation, oldest first, when the list shows conversations.
        let conversation: Vec<MessageSummary> = match self.message.filter(|_| self.threaded()) {
            Some(id) => self.conversations.members(id).iter().filter_map(|m| self.summary_of(*m).cloned()).collect(),
            None => Vec::new(),
        };
        let whole = self.whole_conversation;
        let work_offline = self.work_offline;
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
        let full_preview = self.full_preview && !in_tab;
        let mut back = false;
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
                if full_preview {
                    back = icons::button(ui, t, Icon::ArrowLeft, "Back to the list (Esc)", true, false).clicked();
                    ui.add_space(8.0);
                }
                let archived = role == Role::Archive;
                let loaded = self.body.is_some();
                let ctx = ui.ctx().clone();
                let answers = [
                    (Icon::Reply, Command::Reply, Action::Reply),
                    (Icon::ReplyAll, Command::ReplyAll, Action::ReplyAll),
                    (Icon::Forward, Command::Forward, Action::Forward),
                ];
                for (icon, command, what) in answers {
                    if icons::button(ui, t, icon, &command.tip(&ctx), loaded, false).clicked() {
                        action = Some((message.id, what));
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
                        action = Some((message.id, what));
                    }
                }
                // In Sent and Drafts the sender is the user.
                let blockable = !matches!(role, Role::Junk | Role::Sent | Role::Drafts) && !message.address.is_empty();
                let tip = match role {
                    _ if blockable => "Block sender…",
                    Role::Junk => "Already in Spam",
                    _ => "Your own mail",
                };
                if icons::button(ui, t, Icon::Ban, tip, blockable, false).clicked() {
                    block = Some(message.id);
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
                            ui.set_max_width(ui.available_width().min(720.0));
                            ui.add_space(20.0);
                            ui.heading(&message.subject);
                            ui.add_space(12.0);
                            if conversation.len() > 1 {
                                // The latest few, so a long conversation does not push the message out of sight.
                                let hidden = if whole || conversation.len() <= 6 { 0 } else { conversation.len() - 5 };
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{} messages in this conversation", conversation.len())).size(13.0).color(t.text_dim));
                                    if hidden > 0 && ui.small_button(format!("Show {hidden} earlier")).clicked() {
                                        expand = true;
                                    }
                                });
                                ui.add_space(2.0);
                                ui.spacing_mut().item_spacing.y = 2.0;
                                for m in conversation.iter().skip(hidden) {
                                    if views::thread_row(ui, t, m, m.id == message.id).clicked() && m.id != message.id {
                                        switch = Some(m.id);
                                    }
                                }
                                ui.spacing_mut().item_spacing.y = 8.0;
                                ui.add_space(14.0);
                            }
                            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), views::AVATAR), egui::Sense::hover());
                            views::avatar(ui, t, &message.from, rect.left_center() + vec2(views::AVATAR / 2.0, 0.0), views::AVATAR);
                            let authenticity = sender::authenticity(&body.data.auth, &server_host, &message.address);
                            suspicious = body.findings.iter().any(|f| f.level == screen::Level::Danger);
                            // Never for spam: leaving a list it sends only tells the spammer the address is read.
                            let way = unsubscribe::method(&body.data.unsubscribe, body.data.one_click, authenticity == sender::Authenticity::Verified)
                                .filter(|_| role != Role::Junk && !suspicious && !work_offline);
                            let mut text_right = rect.right();
                            if let Some(way) = way {
                                ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(Layout::right_to_left(Align::Center)), |ui| {
                                    let tip = match &way {
                                        unsubscribe::Method::OneClick(_) => format!("Ask {} to stop sending this mail", way.host()),
                                        unsubscribe::Method::Mail(..) => format!("Write the email {} asks for to unsubscribe", way.host()),
                                        unsubscribe::Method::Page(_) => format!("Open the unsubscribe page of {}", way.host()),
                                    };
                                    let button = ui.button("Unsubscribe").on_hover_text(tip);
                                    text_right = button.rect.left() - 12.0;
                                    if button.clicked() {
                                        leave = Some((message.from.clone(), way));
                                    }
                                });
                            }
                            let left = rect.left() + views::AVATAR + 12.0;
                            let detail = if message.address.is_empty() || message.address == message.from { message.received.clone() } else { format!("{}  ·  {}", message.address, message.received) };
                            let name = views::truncated(ui, &message.from, egui::FontId::proportional(15.5), t.text, text_right - left);
                            let date = views::truncated(ui, &detail, egui::FontId::proportional(13.0), t.text_dim, text_right - left);
                            ui.painter().galley(pos2(left, rect.top() + 3.0), name, t.text);
                            ui.painter().galley(pos2(left, rect.top() + 23.0), date, t.text_dim);
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
                            if let Some(choice) = correction {
                                let key = screen::sender_key(&message.address);
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
                                    Some(views::Mark::Correct) => mark = Some(choice),
                                    Some(views::Mark::Trust) => trust = Some(message.id),
                                    None => {}
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
                                // Working offline nothing is fetched, whatever was allowed.
                                let remote = !work_offline && (body.remote || (settings_mode.allows_remote(&message.address) && !suspicious));
                                if !body.original.as_ref().is_some_and(|p| p.fits(width, scale, remote)) {
                                    body.original = original::Page::new(ui.ctx(), source, width, scale, remote, &body.trackers);
                                }
                                if body.has_remote {
                                    match views::remote_bar(ui, t, remote, if suspicious { "" } else { &message.address }, &body.trackers) {
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
        // A message tab has the window to itself, a message without the reading pane the list's place;
        // otherwise the reader is the pane on the right.
        if in_tab || full_preview {
            egui::CentralPanel::default().frame(frame).show(ui, draw);
        } else {
            let reader = egui::Panel::right("reader").frame(frame).default_size(470.0).size_range(360.0..=900.0).show(ui, draw);
            if self.pane == Some(Pane::Reader) {
                pane_ring(ui, t, reader.response.rect);
            }
        }
        ReaderOutcome { action, url, mode: chosen_mode, remember, attachment, save_many, allow_sender, mark, bulk, unsubscribe: leave, block, switch, expand, back, trust }
    }
}

impl eframe::App for BeaverApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // What waited for Undo is done now, or the mail would come back at the next start.
        if let Some(pending) = self.pending.take() {
            let jobs: Vec<_> = pending.items.into_iter().filter_map(|(serial, _, action, m, _)| Some((self.view(serial)?.source.clone()?, m.id, action))).collect();
            self.runtime.block_on(async {
                let all = async {
                    for (source, id, action) in jobs {
                        let _ = perform(source, id, action).await;
                    }
                };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(10), all).await;
            });
        }
        let _ = std::fs::remove_dir_all(&self.open_dir); // the decrypted copies made to open attachments
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }
        // Closing the app with a message being written asks first, as Discard does. One being sent, or
        // waiting out Undo send, is kept as a draft on the way out without asking.
        if ui.ctx().input(|i| i.viewport().close_requested())
            && self.compose.as_ref().is_some_and(|c| !c.is_empty() && !c.sending)
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            (self.confirm_discard, self.quitting) = (true, true);
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
        let SidebarOutcome { chosen, suspicious, recheck, empty, removed, edit, mut add, all, scheduled } =
            if self.in_tab { SidebarOutcome::default() } else { self.sidebar(ui, &t) };
        if all {
            self.show_suspicious = false;
            self.show_all_inboxes();
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
        self.confirm_remove = removed.or(self.confirm_remove);

        self.handle_keys(ui.ctx());
        if self.full_preview && (self.message.is_none() || self.settings.reading_pane) {
            self.full_preview = false;
        }
        // Without the reading pane and with no message in its place, there is no reader to draw.
        let shows_reader = self.in_tab || self.full_preview || self.settings.reading_pane;
        let ReaderOutcome { action, url, mode, remember, attachment, save_many, allow_sender, mark, bulk, unsubscribe, block, switch, expand, back, trust } =
            if shows_reader { self.reader(ui, &t) } else { ReaderOutcome::default() };
        self.full_preview &= !back;
        if let Some(id) = trust {
            self.trust(id);
        }
        self.whole_conversation |= expand;
        // Another message of the conversation takes the place of the one shown, in the pane or its tab.
        if let Some(id) = switch
            && let Some(at) = self.active()
            && let Some(message) = self.summary_of(id).cloned()
        {
            let role = if self.sent.iter().any(|m| m.id == id) { Role::Sent } else { self.role() };
            let (serial, pinned) = (self.serial_of(id).unwrap_or(self.tabs[at].serial), self.tabs[at].pinned);
            self.tabs[at] = tabs::Tab { serial, message, role, pinned };
            self.activate(at);
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
        let (opened, add_from_list) = if self.in_tab || self.full_preview { (None, false) } else { self.message_list(ui, &t) };
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
        // After Settings, so the dialogs it opens show above it.
        self.account_form(ui.ctx());
        self.remove_dialog(ui.ctx(), &t);
        self.unsubscribe_dialog(ui.ctx(), &t);
        self.palette_window(ui.ctx(), &t);
        self.toast(ui.ctx(), &t);
        self.block_dialog(ui.ctx(), &t);
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
            // egui's 0.3 s is quicker than the systems' (0.4 to 0.5 s), and it is timed by frame, so a slow
            // frame after the first click could split a double click in two.
            cc.egui_ctx.options_mut(|o| o.input_options.max_double_click_delay = 0.5);
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
