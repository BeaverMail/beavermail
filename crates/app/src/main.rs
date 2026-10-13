// SPDX-License-Identifier: AGPL-3.0-only

//! BeaverMail desktop shell: accounts with their folders, a message list and a reader.
//! Accounts come from `accounts.toml` and the system credential store (see
//! `beavermail-accounts`). Mail calls run on a background runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

mod compose;
mod html;
mod icons;
mod theme;
mod views;

use beavermail_accounts as store;
use beavermail_core::{Account, DisplayMode, MailError, MailSender, MailSource, Mailbox, MessageBody, MessageId, MessageSummary, Role, Server, Settings, SyncReport, Tls};
use beavermail_imap::ImapSource;
use beavermail_smtp::SmtpSender;
use beavermail_store::Store;
use beavermail_vault::{SystemStore, Vault};
use eframe::egui::{self, Align, Layout, RichText, pos2, vec2};

use crate::icons::Icon;
use tokio::runtime::Runtime;

/// Result of a background mail call. Tagged with the account's serial, so answers
/// for an account that was removed meanwhile are dropped.
enum Event {
    Mailboxes(u64, Result<Vec<Mailbox>, MailError>),
    /// One sync of the selected folder finished; more history may remain.
    Synced(u64, usize, Result<SyncReport, MailError>),
    Messages(u64, usize, Result<Vec<MessageSummary>, MailError>),
    Body(u64, MessageId, Result<MessageBody, MailError>),
    /// A write (read flag, archive, trash) finished; the UI already showed its result.
    Done(u64, usize, Result<(), MailError>),
    /// The message in the compose window was sent, or the server refused it.
    /// Second field: why the copy in Sent could not be saved, when the message itself went out.
    Sent(Result<(), MailError>, Option<String>),
}

#[derive(Clone, Copy)]
enum Action {
    Unread(bool),
    Archive,
    Trash,
    Erase,
    Reply,
    ReplyAll,
    Forward,
}

impl Action {
    /// Whether the message leaves the folder being shown.
    fn removes(self) -> bool {
        matches!(self, Self::Archive | Self::Trash | Self::Erase)
    }
}

/// What the user did in the sidebar this frame.
struct SidebarOutcome {
    chosen: Option<(u64, usize)>,
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
}

impl OpenBody {
    fn new(data: MessageBody) -> Self {
        let plain = html::plain_blocks(&data.text);
        let simplified = data.html.as_deref().map(html::blocks);
        Self { data, plain, simplified }
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
    confirm_erase: Option<MessageId>,
    /// Text of the search box; filters the loaded messages by sender and subject.
    query: String,
    focus_search: bool,
    compose: Option<compose::Compose>,
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
    /// Display mode chosen for the open message only; forgotten when another is opened.
    view_mode: Option<DisplayMode>,
}

impl BeaverApp {
    fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let (archive, archive_error) = match store::archive_path().and_then(|path| Store::open(&path).map_err(|e| e.to_string())) {
            Ok(archive) => (archive, None),
            // Without a usable file the app still works, but nothing is kept between runs.
            Err(e) => (Store::in_memory().expect("in-memory archive"), Some(format!("The local mail archive is unavailable: {e}"))),
        };
        let (vault, vault_error) = match store::data_dir().and_then(|dir| Vault::open(&dir, &SystemStore).map_err(|e| e.to_string())) {
            Ok(vault) => (vault, None),
            // Never a plain-text fallback: without the key, messages are kept in memory only.
            Err(e) => (Vault::in_memory(), Some(format!("Downloaded messages cannot be stored safely: {e}"))),
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
            confirm_erase: None,
            query: String::new(),
            focus_search: false,
            compose: None,
            confirm_discard: false,
            notice: None,
            settings: Settings::default(),
            archive: Arc::new(archive),
            vault: Arc::new(vault),
            syncing: 0,
            show_settings: false,
            view_mode: None,
        };
        app.error = archive_error.or(vault_error);
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
            self.spawn(async move { Event::Mailboxes(serial, cached.cached_mailboxes().await) });
            self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await) });
        }
        let view = AccountView { serial, account, source, sender, mailboxes: Vec::new(), error };
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
        self.message = None;
        self.body = None;
        self.messages.clear();
        let local = source.clone();
        let folder = name.clone();
        self.spawn(async move { Event::Messages(serial, index, local.messages(&folder).await) });
        self.start_sync(serial, index, source, name);
    }

    /// Brings a folder of the archive up to date in the background.
    fn start_sync(&mut self, serial: u64, index: usize, source: Arc<ImapSource>, folder: String) {
        self.syncing += 1;
        self.spawn(async move { Event::Synced(serial, index, source.sync(&folder).await) });
    }

    fn select_message(&mut self, id: MessageId) {
        let Some((serial, _)) = self.selected else { return };
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        self.message = Some(id);
        self.body = None;
        self.view_mode = None;
        self.spawn(async move { Event::Body(serial, id, source.body(id).await) });
        if self.messages.iter().any(|m| m.id == id && m.unread) {
            self.act(id, Action::Unread(false));
        }
    }

    /// Applies a write to the list at once and sends it to the server; a failure reloads the folder.
    fn act(&mut self, id: MessageId, action: Action) {
        let Some((serial, index)) = self.selected else { return };
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        let Some(pos) = self.messages.iter().position(|m| m.id == id) else { return };
        let was_unread = self.messages[pos].unread;
        let unread_change = match action {
            Action::Unread(unread) => {
                self.messages[pos].unread = unread;
                i32::from(unread) - i32::from(was_unread)
            }
            Action::Archive | Action::Trash | Action::Erase => {
                self.messages.remove(pos);
                -i32::from(was_unread)
            }
            Action::Reply | Action::ReplyAll | Action::Forward => return,
        };
        if let Some(mailbox) = self.accounts.iter_mut().find(|v| v.serial == serial).and_then(|v| v.mailboxes.get_mut(index)) {
            mailbox.unread = mailbox.unread.saturating_add_signed(unread_change);
        }
        self.spawn(async move {
            let result = match action {
                Action::Unread(unread) => source.set_unread(id, unread).await,
                Action::Archive => source.archive(id).await,
                Action::Trash => source.trash(id).await,
                Action::Erase => source.erase(id).await,
                Action::Reply | Action::ReplyAll | Action::Forward => Ok(()),
            };
            Event::Done(serial, index, result)
        });
        if action.removes() && self.message == Some(id) {
            // Keep triaging: open the message that took its place.
            self.message = None;
            self.body = None;
            if let Some(next) = self.messages.get(pos).or(self.messages.last()).map(|m| m.id) {
                self.select_message(next);
            }
        }
    }

    /// The loaded messages that match the search box, in list order.
    fn visible(&self) -> Vec<&MessageSummary> {
        let query = self.query.trim().to_lowercase();
        let matches = |m: &MessageSummary| m.from.to_lowercase().contains(&query) || m.subject.to_lowercase().contains(&query);
        self.messages.iter().filter(|m| query.is_empty() || matches(m)).collect()
    }

    /// Asks the server again for the selected folder and its account's folder list.
    fn refresh(&mut self) {
        let Some((serial, index)) = self.selected else { return };
        let Some(view) = self.view(serial) else { return };
        let (Some(source), Some(mailbox)) = (view.source.clone(), view.mailboxes.get(index)) else { return };
        let name = mailbox.name.clone();
        let folders = source.clone();
        self.spawn(async move { Event::Mailboxes(serial, folders.mailboxes().await) });
        self.start_sync(serial, index, source, name);
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
        self.compose = from.map(compose::Compose::new);
    }

    fn compose_window(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        let Some(mut compose) = self.compose.take() else { return };
        let senders = self.senders();
        match compose::window(ctx, t, &mut compose, &senders) {
            compose::Outcome::Keep => {}
            compose::Outcome::Discard if compose.is_empty() => return,
            compose::Outcome::Discard => self.confirm_discard = true,
            compose::Outcome::Send => {
                if let Some(view) = self.view(compose.from) {
                    let (account, sender, source) = (view.account.clone(), view.sender.clone(), view.source.clone());
                    if let Some(sender) = sender {
                        let message = compose.outgoing(account.address());
                        compose.sending = true;
                        compose.error = None;
                        self.spawn(async move {
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
                return;
            }
            if keep || modal.should_close() {
                self.confirm_discard = false;
            }
        }
        self.compose = Some(compose);
    }

    /// The sender's address of the open message.
    fn current_address(&self) -> Option<String> {
        let id = self.message?;
        self.messages.iter().find(|m| m.id == id).map(|m| m.address.clone())
    }

    fn save_settings(&mut self) {
        if let Err(e) = store::save_settings(&self.settings) {
            self.error = Some(e);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context, t: &theme::Tokens) {
        if !self.show_settings {
            return;
        }
        let mut changed = false;
        let modal = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading("Settings");
            ui.add_space(8.0);
            ui.label(RichText::new("Reading").strong());
            ui.label(RichText::new("Most unwanted mail and tracking arrives as HTML, so messages start as text.").color(t.text_dim));
            ui.add_space(6.0);
            let modes = [
                (DisplayMode::Plain, "Plain text", "The safest view. Links stay clickable; images and styling are dropped."),
                (DisplayMode::Simplified, "Simplified HTML", "Keeps headings, lists, quotes and links. Never loads images, fonts or trackers."),
            ];
            for (mode, title, hint) in modes {
                if ui.radio(self.settings.display == mode, title).clicked() && self.settings.display != mode {
                    self.settings.display = mode;
                    changed = true;
                }
                ui.label(RichText::new(hint).size(13.0).color(t.text_dim));
                ui.add_space(4.0);
            }
            ui.add_enabled(false, egui::RadioButton::new(false, "Original HTML")).on_disabled_hover_text("Not available yet");
            ui.label(RichText::new("Needs a full renderer. Not available yet.").size(13.0).color(t.text_dim));
            if !self.settings.sender_display.is_empty() {
                ui.add_space(12.0);
                ui.label(RichText::new("Senders with their own setting").strong());
                let mut forget = None;
                for (address, mode) in &self.settings.sender_display {
                    ui.horizontal(|ui| {
                        let label = if *mode == DisplayMode::Simplified { "Simplified HTML" } else { "Plain text" };
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
        } else if matches!(action, Action::Trash) && self.role() == Role::Trash {
            self.confirm_erase = Some(id);
        } else {
            self.act(id, action);
        }
    }

    /// Opens a reply or a forward of the open message.
    fn answer(&mut self, id: MessageId, action: Action) {
        let Some((serial, _)) = self.selected else { return };
        let (Some(summary), Some(open)) = (self.messages.iter().find(|m| m.id == id), &self.body) else { return };
        let body = &open.data;
        let Some(view) = self.view(serial).filter(|v| v.sender.is_some()) else { return };
        let own = view.account.address();
        let compose = match action {
            Action::Forward => compose::Compose::forward(serial, summary, body),
            _ => compose::Compose::reply(serial, own, summary, body, matches!(action, Action::ReplyAll)),
        };
        if self.compose.is_none() {
            self.compose = Some(compose);
        }
    }

    /// Up/Down move through the list, E archives, Delete trashes, U toggles unread.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.form.is_some() || self.confirm_erase.is_some() || self.compose.is_some() || self.show_settings || ctx.egui_wants_keyboard_input() {
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
        let ids: Vec<MessageId> = self.visible().iter().map(|m| m.id).collect();
        let pos = self.message.and_then(|id| ids.iter().position(|i| *i == id));
        let step = if key(egui::Key::ArrowDown) { Some(1) } else if key(egui::Key::ArrowUp) { Some(-1) } else { None };
        if let Some(step) = step {
            let next = pos.map_or(0, |p| p.saturating_add_signed(step)).min(ids.len().saturating_sub(1));
            if let Some(id) = ids.get(next).copied().filter(|id| Some(*id) != self.message) {
                self.select_message(id);
            }
        }
        let (Some(id), Some(_)) = (self.message, pos) else { return };
        if key(egui::Key::R) {
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
            let unread = self.messages.iter().any(|m| m.id == id && m.unread);
            self.request(id, Action::Unread(!unread));
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
            Event::Mailboxes(serial, result) => {
                let selected_name = self.selected.filter(|(s, _)| *s == serial).and_then(|(_, i)| self.view(serial)?.mailboxes.get(i)).map(|m| m.name.clone());
                let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) else { return };
                match result {
                    Ok(mut mailboxes) => {
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
                }
                Err(e) => self.error = Some(e.to_string()),
            },
            Event::Body(serial, id, result)
                if self.selected.is_some_and(|(s, _)| s == serial) && self.message == Some(id) =>
            {
                match result {
                    Ok(body) => self.body = Some(OpenBody::new(body)),
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
            Event::Sent(result, copy_error) => match result {
                Ok(()) => {
                    self.compose = None;
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
        let Some(id) = self.confirm_erase else { return };
        let (mut confirm, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm_erase")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH * 0.8);
            ui.heading("Delete permanently?");
            ui.label(RichText::new("This message will be erased from the server and cannot be recovered.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                let erase = egui::Button::new(RichText::new("Delete permanently").color(t.on_accent)).fill(t.danger);
                confirm = ui.add(erase).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            self.act(id, Action::Erase);
        }
        if confirm || cancel || modal.should_close() {
            self.confirm_erase = None;
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
        let (mut chosen, mut removed, mut edit, mut add) = (None, None, None, false);
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
                                        let selected = self.selected == Some((view.serial, i));
                                        if views::folder_row(ui, t, &title, mailbox.role, mailbox.unread, selected).clicked() {
                                            chosen = Some((view.serial, i));
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
        SidebarOutcome { chosen, removed, edit, add }
    }

    /// Search, refresh and the message list. Returns the message the user clicked and whether to add an account.
    fn message_list(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> (Option<MessageId>, bool) {
        let (mut opened, mut add, mut refresh) = (None, false, false);
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
                views::search_box(ui, t, &mut self.query, "Search mail  ( / )", width, focus);
                if self.syncing > 0 {
                    ui.add_sized([theme::CONTROL_H - 4.0, theme::CONTROL_H - 4.0], egui::Spinner::new().size(16.0)).on_hover_text("Syncing…");
                } else {
                    refresh = icons::button(ui, t, Icon::Refresh, "Refresh", true, false).clicked();
                }
            });
            ui.add_space(8.0);
            let shown = self.visible();
            if shown.is_empty() {
                let (title, hint) = if self.query.trim().is_empty() {
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
                    if views::message_row(ui, t, message, self.message == Some(message.id)).clicked() {
                        opened = Some(message.id);
                    }
                }
            });
        });
        if refresh {
            self.refresh();
        }
        (opened, add)
    }

    /// The open message with its action bar. Returns the action the user chose.
    fn reader(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) -> ReaderOutcome {
        let mut action = None;
        let mut url = None;
        let mut chosen_mode = None;
        let mut remember = None;
        let (settings_mode, view_mode) = (self.settings.clone(), self.view_mode);
        let role = self.role();
        let current = self.message.and_then(|id| self.messages.iter().find(|m| m.id == id)).cloned();
        let frame = egui::Frame::new().fill(t.surface);
        egui::Panel::right("reader").frame(frame).default_size(470.0).size_range(360.0..=900.0).show(ui, |ui| {
            let Some(message) = current else {
                views::empty_state(ui, t, "No message selected", "Choose a message from the list to read it.", None);
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
            match &self.body {
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
                            if !body.data.to.is_empty() {
                                ui.add_space(6.0);
                                ui.add(egui::Label::new(RichText::new(format!("To: {}", body.data.to.join(", "))).size(13.0).color(t.text_dim)).truncate());
                            }
                            let stored = settings_mode.mode_for(&message.address);
                            let mode = view_mode.unwrap_or(stored);
                            if body.simplified.is_some() {
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Show as").size(13.0).color(t.text_dim));
                                    for (label, value) in [("Text", DisplayMode::Plain), ("Simplified", DisplayMode::Simplified)] {
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
                            let blocks = match (&body.simplified, mode) {
                                (Some(simplified), DisplayMode::Simplified) => simplified,
                                _ => &body.plain,
                            };
                            url = views::body(ui, t, blocks);
                            ui.add_space(28.0);
                        });
                    });
                }
            }
        });
        ReaderOutcome { action, url, mode: chosen_mode, remember }
    }
}

impl eframe::App for BeaverApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }
        let t = theme::Tokens::get(ui.ctx());
        self.error_banner(ui, &t);

        let SidebarOutcome { chosen, removed, edit, mut add } = self.sidebar(ui, &t);
        if let Some(serial) = edit
            && let Some(view) = self.view(serial)
        {
            self.form = Some(NewAccount::editing(serial, &view.account));
        }
        if let Some((serial, index)) = chosen {
            self.select_mailbox(serial, index);
        }
        if let Some(serial) = removed {
            self.remove_account(serial);
        }
        self.account_form(ui.ctx());

        self.handle_keys(ui.ctx());
        let ReaderOutcome { action, url, mode, remember } = self.reader(ui, &t);
        if let Some(url) = url {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if mode.is_some() {
            self.view_mode = mode;
        }
        if let (Some(mode), Some(address)) = (remember, self.current_address()) {
            self.settings.set_sender_mode(&address, mode);
            self.view_mode = None;
            self.save_settings();
        }
        let (opened, add_from_list) = self.message_list(ui, &t);
        add |= add_from_list;

        if let Some(id) = opened {
            self.select_message(id);
        }
        if let Some((id, what)) = action {
            self.request(id, what);
        }
        self.erase_dialog(ui.ctx(), &t);
        self.settings_window(ui.ctx(), &t);
        self.compose_window(ui.ctx(), &t);
        if add {
            self.form = Some(NewAccount::default());
        }
    }
}

fn main() -> eframe::Result {
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

#[cfg(test)]
mod tests {
    use super::*;

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
