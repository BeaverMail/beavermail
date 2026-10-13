//! The "New message" window: addresses, subject and a plain-text body.

use beavermail_core::Outgoing;
use eframe::egui::{self, Align, Align2, Frame, Key, Layout, Modifiers, RichText, TextEdit, Ui, vec2};

use crate::icons::Icon;
use crate::theme::{self, Tokens};
use crate::views;

const ROW_H: f32 = 38.0;
const LABEL_W: f32 = 76.0;

pub struct Compose {
    /// Serial of the sending account.
    pub from: u64,
    to: String,
    cc: String,
    subject: String,
    body: String,
    pub sending: bool,
    pub error: Option<String>,
    focus_to: bool,
}

pub enum Outcome {
    Keep,
    Send,
    Discard,
}

impl Compose {
    pub fn new(from: u64) -> Self {
        Self { from, to: String::new(), cc: String::new(), subject: String::new(), body: String::new(), sending: false, error: None, focus_to: true }
    }

    /// Nothing typed yet, so closing loses nothing.
    pub fn is_empty(&self) -> bool {
        [&self.to, &self.cc, &self.subject, &self.body].iter().all(|t| t.trim().is_empty())
    }

    pub fn outgoing(&self, from_address: &str) -> Outgoing {
        Outgoing {
            from: from_address.to_owned(),
            to: addresses(&self.to),
            cc: addresses(&self.cc),
            subject: self.subject.trim().to_owned(),
            body: self.body.clone(),
        }
    }
}

/// Splits a recipient field on commas, semicolons and newlines.
// shortcut: a comma inside a quoted display name ("Doe, John" <j@x.org>) splits it wrongly; write a real parser with contacts.
pub fn addresses(text: &str) -> Vec<String> {
    text.split([',', ';', '\n']).map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned).collect()
}

/// A dim label in a fixed-width column, left-aligned so the fields start at the same x.
fn label(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.allocate_ui_with_layout(vec2(LABEL_W, ROW_H), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(vec2(LABEL_W, ROW_H));
        ui.label(RichText::new(text).color(t.text_dim));
    });
}

/// A labelled, borderless single-line field followed by a divider.
fn row(ui: &mut Ui, t: &Tokens, name: &str, text: &mut String, hint: &str) -> egui::Response {
    let response = ui
        .horizontal(|ui| {
            label(ui, t, name);
            ui.add(
                TextEdit::singleline(text)
                    .frame(Frame::NONE)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY)
                    .min_size(vec2(0.0, ROW_H))
                    .vertical_align(Align::Center),
            )
        })
        .inner;
    ui.separator();
    response
}

/// Shows the window. `senders` lists the accounts that can send, as (serial, address).
pub fn window(ctx: &egui::Context, t: &Tokens, compose: &mut Compose, senders: &[(u64, String)]) -> Outcome {
    let mut outcome = Outcome::Keep;
    let ready = !compose.sending && !compose.to.trim().is_empty();
    egui::Window::new("New message")
        .id(egui::Id::new("compose"))
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
        .default_width(620.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.add_enabled_ui(!compose.sending, |ui| {
                ui.horizontal(|ui| {
                    label(ui, t, "From");
                    let current = senders.iter().find(|(s, _)| *s == compose.from).map(|(_, a)| a.as_str()).unwrap_or("");
                    egui::ComboBox::from_id_salt("compose_from").selected_text(current).show_ui(ui, |ui| {
                        for (serial, address) in senders {
                            ui.selectable_value(&mut compose.from, *serial, address);
                        }
                    });
                });
                ui.separator();
                let to = row(ui, t, "To", &mut compose.to, "name@example.com, …");
                if std::mem::take(&mut compose.focus_to) {
                    to.request_focus();
                }
                row(ui, t, "Cc", &mut compose.cc, "");
                row(ui, t, "Subject", &mut compose.subject, "");
                // Leave room for the header rows and buttons so the window never outgrows a small screen.
                let body_h = (ctx.content_rect().height() - 360.0).clamp(120.0, 420.0);
                egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, true]).show(ui, |ui| {
                    ui.add(
                        TextEdit::multiline(&mut compose.body)
                            .frame(Frame::NONE)
                            .hint_text("Write your message")
                            .desired_width(f32::INFINITY)
                            .desired_rows(8),
                    );
                });
            });
            if let Some(error) = &compose.error {
                ui.add_space(4.0);
                ui.colored_label(t.danger, error);
            }
            ui.add_space(8.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                let send = views::primary_wide_fixed(ui, t, Icon::Send, if compose.sending { "Sending…" } else { "Send" }, ready, 120.0);
                if send.clicked() {
                    outcome = Outcome::Send;
                }
                if compose.sending {
                    ui.spinner();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!compose.sending, egui::Button::image_and_text(Icon::Trash.image(crate::icons::SIZE, t.text_dim), "Discard")).clicked() {
                        outcome = Outcome::Discard;
                    }
                    ui.label(RichText::new("Ctrl+Enter to send").size(12.5).color(t.text_dim));
                });
            });
        });
    if ready && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
        outcome = Outcome::Send;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_fields_split_on_separators() {
        assert_eq!(addresses("a@x.org, b@x.org;\n c@x.org ,,"), ["a@x.org", "b@x.org", "c@x.org"]);
        assert!(addresses("  ").is_empty());
    }

    #[test]
    fn empty_means_nothing_typed() {
        let mut c = Compose::new(0);
        assert!(c.is_empty());
        c.subject = "Hi".into();
        assert!(!c.is_empty());
    }
}
