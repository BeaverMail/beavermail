// SPDX-License-Identifier: AGPL-3.0-only

//! BeaverMail desktop shell: a three-pane mail view (folders, message list, reader)
//! over a [`MailSource`]. Milestone 0: sample data only, no protocols yet.

use beavermail_core::sample::SampleSource;
use beavermail_core::{MailSource, Mailbox, MessageId, MessageSummary};
use eframe::egui::{self, Align, Layout, RichText, vec2};

/// Fixed row height, so the list can be virtualised with `show_rows`.
const ROW_H: f32 = 52.0;
/// Number of generated messages in the sample source.
const SAMPLE_MESSAGES: usize = 10_000;

struct BeaverApp {
    source: SampleSource,
    mailboxes: Vec<Mailbox>,
    selected_mailbox: usize,
    messages: Vec<MessageSummary>,
    selected: Option<MessageId>,
}

impl BeaverApp {
    fn new(source: SampleSource) -> Self {
        let mailboxes = source.mailboxes();
        let messages = source.messages(&mailboxes[0].name);
        Self { source, mailboxes, selected_mailbox: 0, messages, selected: None }
    }

    fn select_mailbox(&mut self, index: usize) {
        self.selected_mailbox = index;
        self.messages = self.source.messages(&self.mailboxes[index].name);
        self.selected = None;
    }
}

impl eframe::App for BeaverApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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

        let mut opened = None;
        egui::Panel::right("reader").default_size(460.0).show(ui, |ui| match self.selected {
            Some(id) => {
                let body = self.source.body(id).unwrap_or_default();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.label(RichText::new(body).size(14.0));
                });
            }
            None => {
                ui.centered_and_justified(|ui| ui.weak("Select a message"));
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let mut clicked = None;
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
                                    clicked = Some(message.id);
                                }
                            },
                        );
                    }
                },
            );
            opened = clicked;
        });

        if let Some(id) = opened {
            self.selected = Some(id);
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
        Box::new(|_cc| Ok(Box::new(BeaverApp::new(SampleSource::new(SAMPLE_MESSAGES))))),
    )
}
