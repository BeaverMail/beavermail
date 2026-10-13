//! The compose window (new message, reply, forward): addresses, subject and a plain-text body.

use beavermail_core::{MessageBody, MessageSummary, Outgoing};
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
    /// Window title: "New message", "Reply" or "Forward".
    title: &'static str,
    in_reply_to: Option<String>,
    references: Vec<String>,
    pub sending: bool,
    pub error: Option<String>,
    focus_to: bool,
    /// Put the caret at the top of the body on the first frame (replies and forwards).
    focus_body: bool,
}

pub enum Outcome {
    Keep,
    Send,
    Discard,
}

impl Compose {
    pub fn new(from: u64) -> Self {
        Self {
            from,
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            title: "New message",
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
            error: None,
            focus_to: true,
            focus_body: false,
        }
    }

    /// A reply to `message`, to the sender only or to everyone but `own` (the sending address).
    pub fn reply(from: u64, own: &str, message: &MessageSummary, body: &MessageBody, all: bool) -> Self {
        let mut to: Vec<String> = if body.reply_to.is_empty() { vec![sender(message)] } else { body.reply_to.clone() };
        let mut cc: Vec<String> = Vec::new();
        if all {
            let seen: Vec<String> = to.iter().map(|a| bare_address(a)).collect();
            for other in body.to.iter().chain(&body.cc) {
                let bare = bare_address(other);
                if bare != bare_address(own) && !seen.contains(&bare) && !cc.iter().any(|c| bare_address(c) == bare) {
                    cc.push(other.clone());
                }
            }
        }
        to.retain(|a| !a.trim().is_empty());
        let mut references = body.references.clone();
        if !body.message_id.is_empty() {
            references.push(body.message_id.clone());
        }
        Self {
            to: to.join(", "),
            cc: cc.join(", "),
            subject: prefixed("Re", &message.subject),
            body: format!("\n\nOn {}, {} wrote:\n{}", message.received, sender(message), quote(&body.text)),
            title: if all { "Reply all" } else { "Reply" },
            in_reply_to: Some(body.message_id.clone()).filter(|id| !id.is_empty()),
            references,
            focus_to: false,
            focus_body: true,
            ..Self::new(from)
        }
    }

    /// `message` forwarded inline. Attachments are not carried over yet.
    pub fn forward(from: u64, message: &MessageSummary, body: &MessageBody) -> Self {
        let mut header = format!("---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\n", sender(message), message.received, message.subject);
        if !body.to.is_empty() {
            header.push_str(&format!("To: {}\n", body.to.join(", ")));
        }
        Self {
            subject: prefixed("Fwd", &message.subject),
            body: format!("\n\n{header}\n{}", body.text),
            title: "Forward",
            focus_to: true,
            ..Self::new(from)
        }
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
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
        }
    }
}

/// "Name <address>" for a message's sender.
fn sender(message: &MessageSummary) -> String {
    match (message.from.trim(), message.address.trim()) {
        ("", address) => address.to_owned(),
        (name, "") => name.to_owned(),
        (name, address) if name == address => address.to_owned(),
        (name, address) => format!("{name} <{address}>"),
    }
}

/// The lower-case address inside "Name <address>", or the text itself.
fn bare_address(text: &str) -> String {
    let inner = match (text.rfind('<'), text.rfind('>')) {
        (Some(open), Some(close)) if open < close => &text[open + 1..close],
        _ => text,
    };
    inner.trim().to_lowercase()
}

/// "Re: subject" without stacking prefixes.
fn prefixed(prefix: &str, subject: &str) -> String {
    let subject = subject.trim();
    let has = subject.len() > prefix.len() && subject[..prefix.len()].eq_ignore_ascii_case(prefix) && subject[prefix.len()..].starts_with(':');
    if has { subject.to_owned() } else { format!("{prefix}: {subject}") }
}

/// Every line of `text` marked as a quotation.
fn quote(text: &str) -> String {
    text.trim_end().lines().map(|l| if l.is_empty() { ">".to_owned() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
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
    egui::Window::new(compose.title)
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
                    let id = egui::Id::new("compose_body");
                    let mut output = TextEdit::multiline(&mut compose.body)
                        .id(id)
                        .frame(Frame::NONE)
                        .hint_text("Write your message")
                        .desired_width(f32::INFINITY)
                        .desired_rows(8)
                        .show(ui);
                    if std::mem::take(&mut compose.focus_body) {
                        output.response.request_focus();
                        output.state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(0))));
                        output.state.store(ctx, id);
                    }
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

    fn message() -> (MessageSummary, MessageBody) {
        let summary = MessageSummary {
            id: beavermail_core::MessageId(1),
            from: "Ada Lovelace".into(),
            address: "ada@example.org".into(),
            subject: "Lunch".into(),
            preview: String::new(),
            received: "2026-10-13 12:00".into(),
            timestamp: 0,
            has_attachments: false,
            unread: false,
        };
        let body = MessageBody {
            text: "See you at noon.\n\nBring the notes.".into(),
            to: vec!["me@example.org".into(), "Grace <grace@example.org>".into()],
            cc: vec!["Linus <linus@example.org>".into(), "GRACE@example.org".into()],
            reply_to: vec![],
            message_id: "m2@host".into(),
            references: vec!["m1@host".into()],
            html: None,
            attachments: Vec::new(),
        };
        (summary, body)
    }

    #[test]
    fn a_reply_goes_to_the_sender_and_keeps_the_thread() {
        let (summary, body) = message();
        let c = Compose::reply(0, "me@example.org", &summary, &body, false);
        assert_eq!(c.to, "Ada Lovelace <ada@example.org>");
        assert!(c.cc.is_empty());
        assert_eq!(c.subject, "Re: Lunch");
        assert!(c.body.starts_with("\n\nOn 2026-10-13 12:00, Ada Lovelace <ada@example.org> wrote:\n> See you at noon.\n>\n> Bring the notes."), "{}", c.body);
        let out = c.outgoing("me@example.org");
        assert_eq!(out.in_reply_to.as_deref(), Some("m2@host"));
        assert_eq!(out.references, ["m1@host", "m2@host"]);
    }

    #[test]
    fn reply_all_drops_me_and_duplicates() {
        let (summary, body) = message();
        let c = Compose::reply(0, "Me@Example.org", &summary, &body, true);
        assert_eq!(c.to, "Ada Lovelace <ada@example.org>");
        assert_eq!(c.cc, "Grace <grace@example.org>, Linus <linus@example.org>");
    }

    #[test]
    fn reply_to_header_wins_and_prefixes_do_not_stack() {
        let (mut summary, mut body) = message();
        body.reply_to = vec!["list@example.org".into()];
        summary.subject = "RE: Lunch".into();
        let c = Compose::reply(0, "me@example.org", &summary, &body, false);
        assert_eq!(c.to, "list@example.org");
        assert_eq!(c.subject, "RE: Lunch");
    }

    #[test]
    fn a_forward_quotes_the_original_without_threading() {
        let (summary, body) = message();
        let c = Compose::forward(0, &summary, &body);
        assert_eq!(c.subject, "Fwd: Lunch");
        assert!(c.to.is_empty());
        assert!(c.body.contains("---------- Forwarded message ----------\nFrom: Ada Lovelace <ada@example.org>"));
        assert!(c.body.ends_with("Bring the notes."));
        assert!(c.outgoing("me@example.org").in_reply_to.is_none());
    }

    #[test]
    fn empty_means_nothing_typed() {
        let mut c = Compose::new(0);
        assert!(c.is_empty());
        c.subject = "Hi".into();
        assert!(!c.is_empty());
    }
}
