// SPDX-License-Identifier: AGPL-3.0-only

//! BeaverMail desktop shell: a three-pane mail view (folders, message list, reader)
//! over a [`MailSource`]. Uses the IMAP account in the environment (or `.env`) when
//! set, otherwise generated sample data. Mail calls run on a background runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use beavermail_core::sample::SampleSource;
use beavermail_core::{MailError, MailSource, Mailbox, MessageId, MessageSummary};
use beavermail_imap::{Config, ImapSource};
use eframe::egui::{self, Align, Layout, RichText, vec2};
use tokio::runtime::Runtime;

/// Fixed row height, so the list can be virtualised with `show_rows`.
const ROW_H: f32 = 52.0;
/// Number of generated messages in the sample source.
const SAMPLE_MESSAGES: usize = 10_000;

/// Result of a background mail call, tagged so stale answers can be dropped.
enum Event {
    Mailboxes(Result<Vec<Mailbox>, MailError>),
    Messages(usize, Result<Vec<MessageSummary>, MailError>),
    Body(MessageId, Result<String, MailError>),
}

struct BeaverApp<S> {
    source: Arc<S>,
    runtime: Runtime,
    ctx: egui::Context,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    mailboxes: Vec<Mailbox>,
    selected_mailbox: usize,
    messages: Vec<MessageSummary>,
    selected: Option<MessageId>,
    body: Option<String>,
    error: Option<MailError>,
}

impl<S: MailSource + Send + Sync + 'static> BeaverApp<S> {
    fn new(source: S, ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let app = Self {
            source: Arc::new(source),
            runtime: Runtime::new().expect("tokio runtime"),
            ctx,
            tx,
            rx,
            mailboxes: Vec::new(),
            selected_mailbox: 0,
            messages: Vec::new(),
            selected: None,
            body: None,
            error: None,
        };
        app.spawn(|source| async move { Event::Mailboxes(source.mailboxes().await) });
        app
    }

    /// Runs one mail call in the background and wakes the UI with its result.
    fn spawn<F, Fut>(&self, call: F)
    where
        F: FnOnce(Arc<S>) -> Fut,
        Fut: Future<Output = Event> + Send + 'static,
    {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let fut = call(Arc::clone(&self.source));
        self.runtime.spawn(async move {
            let _ = tx.send(fut.await); // the receiver is gone only when the app is closing
            ctx.request_repaint();
        });
    }

    fn select_mailbox(&mut self, index: usize) {
        self.selected_mailbox = index;
        self.selected = None;
        self.body = None;
        self.messages.clear();
        let name = self.mailboxes[index].name.clone();
        self.spawn(move |source| async move { Event::Messages(index, source.messages(&name).await) });
    }

    fn select_message(&mut self, id: MessageId) {
        self.selected = Some(id);
        self.body = None;
        self.spawn(move |source| async move { Event::Body(id, source.body(id).await) });
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Mailboxes(Ok(mailboxes)) if !mailboxes.is_empty() => {
                self.mailboxes = mailboxes;
                self.error = None;
                self.select_mailbox(0);
            }
            Event::Mailboxes(Ok(_)) => self.error = Some(MailError::Backend("the account has no mailboxes".into())),
            Event::Messages(index, Ok(messages)) if index == self.selected_mailbox => {
                self.messages = messages;
                self.error = None;
            }
            Event::Body(id, Ok(body)) if self.selected == Some(id) => self.body = Some(body),
            Event::Mailboxes(Err(e)) | Event::Messages(_, Err(e)) | Event::Body(_, Err(e)) => self.error = Some(e),
            _ => {} // answer for a mailbox or message that is no longer selected
        }
    }
}

impl<S: MailSource + Send + Sync + 'static> eframe::App for BeaverApp<S> {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }

        if let Some(error) = &self.error {
            egui::Panel::top("error").show(ui, |ui| {
                ui.colored_label(ui.visuals().error_fg_color, error.to_string());
            });
        }

        let mut chosen_mailbox = None;
        egui::Panel::left("folders").exact_size(200.0).resizable(false).show(ui, |ui| {
            for (i, mailbox) in self.mailboxes.iter().enumerate() {
                let label = if mailbox.unread > 0 {
                    format!("{}  ({})", mailbox.name, mailbox.unread)
                } else {
                    mailbox.name.clone()
                };
                if ui.selectable_label(i == self.selected_mailbox, label).clicked() {
                    chosen_mailbox = Some(i);
                }
            }
        });
        if let Some(index) = chosen_mailbox {
            self.select_mailbox(index);
        }

        egui::Panel::right("reader").default_size(460.0).show(ui, |ui| match (self.selected, &self.body) {
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

fn run<S: MailSource + Send + Sync + 'static>(title: &str, source: S) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(title).with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(title, options, Box::new(|cc| Ok(Box::new(BeaverApp::new(source, cc.egui_ctx.clone())))))
}

fn main() -> eframe::Result {
    let _ = dotenvy::dotenv(); // optional: a missing .env just means sample data
    match Config::from_env() {
        Ok(config) => {
            let title = format!("BeaverMail — {}", config.user);
            run(&title, ImapSource::new(config))
        }
        Err(why) => {
            eprintln!("sample data ({why})");
            run("BeaverMail (sample data)", SampleSource::new(SAMPLE_MESSAGES))
        }
    }
}
