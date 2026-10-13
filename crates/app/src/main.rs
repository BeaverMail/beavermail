// SPDX-License-Identifier: AGPL-3.0-only

//! BeaverMail desktop shell: accounts with their folders, a message list and a reader.
//! Accounts come from `accounts.toml` and the system credential store (see
//! `beavermail-accounts`). Mail calls run on a background runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

mod theme;

use beavermail_accounts as store;
use beavermail_core::{Account, MailError, MailSource, Mailbox, MessageId, MessageSummary, Server, Tls};
use beavermail_imap::ImapSource;
use eframe::egui::{self, Align, Layout, RichText, vec2};
use tokio::runtime::Runtime;

/// Fixed row height, so the list can be virtualised with `show_rows`.
const ROW_H: f32 = 52.0;

/// Result of a background mail call. Tagged with the account's serial, so answers
/// for an account that was removed meanwhile are dropped.
enum Event {
    Mailboxes(u64, Result<Vec<Mailbox>, MailError>),
    Messages(u64, usize, Result<Vec<MessageSummary>, MailError>),
    Body(u64, MessageId, Result<String, MailError>),
}

struct AccountView {
    /// Never reused within a run, unlike a position in the list.
    serial: u64,
    account: Account,
    /// `None` when the password could not be read; `error` says why.
    source: Option<Arc<ImapSource>>,
    mailboxes: Vec<Mailbox>,
    error: Option<String>,
}

/// The "Add account" form. Ports are text until submitted.
struct NewAccount {
    name: String,
    user: String,
    password: String,
    imap_host: String,
    imap_port: String,
    imap_tls: Tls,
    /// Empty means the same host as IMAP.
    smtp_host: String,
    smtp_port: String,
    smtp_tls: Tls,
    error: Option<String>,
    /// Focus the first field on the first frame only.
    focus_first: bool,
}

impl Default for NewAccount {
    fn default() -> Self {
        Self {
            name: String::new(),
            user: String::new(),
            password: String::new(),
            imap_host: String::new(),
            imap_port: "143".into(),
            imap_tls: Tls::StartTls,
            smtp_host: String::new(),
            smtp_port: "587".into(),
            smtp_tls: Tls::StartTls,
            error: None,
            focus_first: true,
        }
    }
}

impl NewAccount {
    fn build(&self) -> Result<Account, String> {
        let port = |text: &str, what: &str| text.trim().parse().map_err(|_| format!("{what} port is not a number"));
        let (user, imap_host) = (self.user.trim(), self.imap_host.trim());
        if user.is_empty() || imap_host.is_empty() || self.password.is_empty() {
            return Err("user, password and IMAP host are required".into());
        }
        let smtp_host = if self.smtp_host.trim().is_empty() { imap_host } else { self.smtp_host.trim() };
        Ok(Account {
            name: if self.name.trim().is_empty() { user.to_owned() } else { self.name.trim().to_owned() },
            user: user.to_owned(),
            imap: Server { host: imap_host.to_owned(), port: port(&self.imap_port, "IMAP")?, tls: self.imap_tls },
            smtp: Server { host: smtp_host.to_owned(), port: port(&self.smtp_port, "SMTP")?, tls: self.smtp_tls },
        })
    }
}

const FORM_WIDTH: f32 = 460.0;
/// Text field width in the account form: the form minus its label column and spacing.
const FIELD_WIDTH: f32 = 330.0;

fn form_grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 12.0]).min_col_width(64.0).show(ui, add);
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
    body: Option<String>,
    error: Option<String>,
    form: Option<NewAccount>,
}

impl BeaverApp {
    fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
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
        };
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
        let serial = self.next_serial;
        self.next_serial += 1;
        let (source, error) = match store::password(&account) {
            Ok(password) => (Some(Arc::new(ImapSource::new(&account, password))), None),
            Err(e) => (None, Some(e)),
        };
        if let Some(source) = source.clone() {
            self.spawn(async move { Event::Mailboxes(serial, source.mailboxes().await) });
        }
        self.accounts.push(AccountView { serial, account, source, mailboxes: Vec::new(), error });
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
        self.spawn(async move { Event::Messages(serial, index, source.messages(&name).await) });
    }

    fn select_message(&mut self, id: MessageId) {
        let Some((serial, _)) = self.selected else { return };
        let Some(source) = self.view(serial).and_then(|v| v.source.clone()) else { return };
        self.message = Some(id);
        self.body = None;
        self.spawn(async move { Event::Body(serial, id, source.body(id).await) });
    }

    /// Stores the password, then the settings, then shows the account.
    fn add_account(&mut self, form: &NewAccount) -> Result<(), String> {
        let account = form.build()?;
        if self.accounts.iter().any(|v| v.account.key() == account.key()) {
            return Err("this account is already configured".into());
        }
        store::set_password(&account, &form.password)?;
        let all: Vec<Account> =
            self.accounts.iter().map(|v| v.account.clone()).chain([account.clone()]).collect();
        if let Err(e) = store::save(&all) {
            let _ = store::forget_password(&account); // do not leave a secret without settings
            return Err(e);
        }
        self.add_view(account);
        Ok(())
    }

    fn remove_account(&mut self, serial: u64) {
        let Some(position) = self.accounts.iter().position(|v| v.serial == serial) else { return };
        let view = self.accounts.remove(position);
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
                let Some(view) = self.accounts.iter_mut().find(|v| v.serial == serial) else { return };
                match result {
                    Ok(mailboxes) => {
                        view.mailboxes = mailboxes;
                        view.error = None;
                        if self.selected.is_none() {
                            self.select_mailbox(serial, 0);
                        }
                    }
                    Err(e) => view.error = Some(e.to_string()),
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
                    Ok(body) => self.body = Some(body),
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            _ => {} // answer for a folder or message that is no longer selected
        }
    }

    fn account_form(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else { return };
        let t = theme::Tokens::get(ctx);
        let (mut submit, mut cancel) = (false, false);
        let ready = !form.user.trim().is_empty() && !form.imap_host.trim().is_empty() && !form.password.is_empty();
        let modal = egui::Modal::new(egui::Id::new("add_account")).show(ctx, |ui| {
            ui.set_width(FORM_WIDTH);
            ui.heading("Add account");
            ui.label(RichText::new("Connect an existing mailbox over IMAP and SMTP.").color(t.text_dim));
            ui.add_space(8.0);
            form_grid(ui, "account_form", |ui| {
                ui.label("Name");
                let name = ui.add(field(&mut form.name).hint_text("Optional"));
                if std::mem::take(&mut form.focus_first) {
                    name.request_focus();
                }
                ui.end_row();
                ui.label("User");
                ui.add(field(&mut form.user).hint_text("name@example.com"));
                ui.end_row();
                ui.label("Password");
                ui.add(field(&mut form.password).password(true));
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
            match self.add_account(&form) {
                Ok(()) => open = false,
                Err(e) => form.error = Some(e),
            }
        }
        if open {
            self.form = Some(form);
        }
    }
}

impl eframe::App for BeaverApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }

        if let Some(error) = &self.error {
            egui::Panel::top("error").show(ui, |ui| {
                ui.colored_label(ui.visuals().error_fg_color, error);
            });
        }

        let (mut chosen, mut removed, mut add) = (None, None, false);
        egui::Panel::left("accounts").default_size(220.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for view in &self.accounts {
                    let header = egui::CollapsingHeader::new(&view.account.name)
                        .id_salt(view.serial)
                        .default_open(true)
                        .show(ui, |ui| {
                            if let Some(error) = &view.error {
                                ui.colored_label(ui.visuals().error_fg_color, error);
                            }
                            for (i, mailbox) in view.mailboxes.iter().enumerate() {
                                let label = if mailbox.unread > 0 {
                                    format!("{}  ({})", mailbox.name, mailbox.unread)
                                } else {
                                    mailbox.name.clone()
                                };
                                if ui.selectable_label(self.selected == Some((view.serial, i)), label).clicked() {
                                    chosen = Some((view.serial, i));
                                }
                            }
                        });
                    header.header_response.context_menu(|ui| {
                        if ui.button("Remove account").clicked() {
                            removed = Some(view.serial);
                            ui.close();
                        }
                    });
                }
                if self.accounts.is_empty() {
                    ui.weak("No accounts yet");
                }
            });
            ui.separator();
            add = ui.button("Add account…").clicked();
        });
        if let Some((serial, index)) = chosen {
            self.select_mailbox(serial, index);
        }
        if let Some(serial) = removed {
            self.remove_account(serial);
        }
        if add {
            self.form = Some(NewAccount::default());
        }
        self.account_form(ui.ctx());

        egui::Panel::right("reader").default_size(460.0).show(ui, |ui| match (self.message, &self.body) {
            (Some(_), Some(body)) => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.label(RichText::new(body).size(14.0));
                });
            }
            (Some(_), None) => {
                ui.centered_and_justified(|ui| ui.weak("Loading…"));
            }
            _ => {
                ui.centered_and_justified(|ui| ui.weak("Select a message"));
            }
        });

        let mut opened = None;
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
                ui,
                ROW_H,
                self.messages.len(),
                |ui, range| {
                    for message in &self.messages[range] {
                        ui.allocate_ui_with_layout(
                            vec2(ui.available_width(), ROW_H),
                            Layout::top_down(Align::LEFT),
                            |ui| {
                                let sender = if message.unread {
                                    RichText::new(&message.from).strong()
                                } else {
                                    RichText::new(&message.from)
                                };
                                let row = ui.horizontal(|ui| {
                                    ui.label(sender);
                                    ui.label(&message.subject);
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.weak(&message.received);
                                    });
                                });
                                ui.weak(&message.preview);
                                if row.response.interact(egui::Sense::click()).clicked() {
                                    opened = Some(message.id);
                                }
                            },
                        );
                    }
                },
            );
        });

        if let Some(id) = opened {
            self.select_message(id);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("BeaverMail").with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "BeaverMail",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(BeaverApp::new(cc.egui_ctx.clone())))
        }),
    )
}
