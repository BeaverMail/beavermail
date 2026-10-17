// SPDX-License-Identifier: AGPL-3.0-only

//! A person's card: who they are to the user (in the address book, or only known from mail), their
//! name and notes, and the latest conversations with them.

use beavermail_core::Hit;
use beavermail_store::ContactCard;
use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::theme::{self, Tokens};
use crate::views;

pub struct Card {
    /// As the archive knows them; `saved` false and nothing sent for someone only seen in mail.
    pub data: ContactCard,
    /// The name and notes being edited.
    pub name: String,
    pub notes: String,
    /// The latest messages from or to them, as (account serial, where it is).
    pub recent: Vec<(u64, Hit)>,
}

pub enum CardAction {
    /// Save the name and notes, putting them in the address book.
    Save,
    /// Take them out of the address book.
    Remove,
    /// Write them a new message.
    Write,
    /// Open this message of the conversations with them, by its place in `recent`.
    Open(usize),
    Close,
}

impl Card {
    pub fn new(data: ContactCard, recent: Vec<(u64, Hit)>) -> Self {
        Self { name: data.name.clone(), notes: data.notes.clone(), data, recent }
    }

    /// The name and notes differ from what is saved.
    fn edited(&self) -> bool {
        self.name.trim() != self.data.name || self.notes.trim_end() != self.data.notes
    }

    /// Who they are to the user, in one line.
    fn standing(&self) -> String {
        let written = match (self.data.sent, self.data.last_sent.and_then(|at| chrono::DateTime::from_timestamp(at, 0))) {
            (0, _) | (_, None) => "You have not written to them".to_owned(),
            (n, Some(at)) => format!("You wrote to them {n} time{}, last on {}", if n == 1 { "" } else { "s" }, at.with_timezone(&chrono::Local).format("%-d %b %Y")),
        };
        match (self.data.saved, self.data.sent) {
            (true, _) => format!("In your contacts  ·  {written}"),
            // Suggested when writing only once written to: someone who only writes to the user is not.
            (false, 0) => format!("Not in your contacts  ·  {written}"),
            (false, _) => format!("Suggested when you write, not in your contacts  ·  {written}"),
        }
    }
}

/// Shows the card over everything else; returns what the user asked for.
pub fn show(ctx: &egui::Context, t: &Tokens, card: &mut Card) -> Option<CardAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("contact_card")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        action = body(ui, t, card, true);
    });
    if modal.should_close() && action.is_none() {
        action = Some(CardAction::Close);
    }
    action
}

/// The card's content, in a dialog (`closable`, with Close) or in the Contacts section.
pub fn body(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, closable: bool) -> Option<CardAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(views::AVATAR, views::AVATAR), egui::Sense::hover());
        let shown = if card.data.name.is_empty() { &card.data.address } else { &card.data.name };
        views::avatar(ui, t, shown, rect.center(), views::AVATAR);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(shown).heading());
            if !card.data.name.is_empty() {
                ui.label(RichText::new(&card.data.address).color(t.text_dim));
            }
        });
    });
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(card.standing()).size(13.0).color(t.text_dim)).wrap());
    ui.add_space(10.0);
    crate::form_grid(ui, "card_fields", |ui| {
        ui.label("Name");
        ui.add(crate::field(&mut card.name).hint_text("As you call them"));
        ui.end_row();
    });
    ui.add_space(8.0);
    ui.label("Notes");
    ui.add(egui::TextEdit::multiline(&mut card.notes).id_salt("card_notes").desired_rows(3).desired_width(f32::INFINITY).margin(egui::Margin::same(12)).hint_text("Only on this computer"));
    if !card.recent.is_empty() {
        ui.add_space(10.0);
        ui.label(RichText::new("Latest conversations").family(theme::bold()));
        ui.spacing_mut().item_spacing.y = 2.0;
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            for (i, (_, hit)) in card.recent.iter().enumerate() {
                if views::thread_row(ui, t, &hit.message, false).on_hover_text("Open this message").clicked() {
                    action = Some(CardAction::Open(i));
                }
            }
        });
    }
    ui.add_space(10.0);
    // What to do with them on a row of its own: with Save and Close beside, four buttons do not fit.
    ui.horizontal(|ui| {
        if ui.button("Write message").clicked() {
            action = Some(CardAction::Write);
        }
        if ui.button("Copy address").clicked() {
            ui.ctx().copy_text(card.data.address.clone());
        }
        if card.data.saved && ui.button(RichText::new("Remove").color(t.danger)).on_hover_text("Take them out of your contacts; they stay a suggestion if you wrote to them").clicked() {
            action = Some(CardAction::Remove);
        }
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
        let (label, enabled) = if card.data.saved { ("Save", card.edited()) } else { ("Save to contacts", true) };
        if ui.add_enabled(enabled, egui::Button::new(RichText::new(label).color(t.on_accent)).fill(t.accent)).clicked() {
            action = Some(CardAction::Save);
        }
        if closable && ui.button("Close").clicked() {
            action = Some(CardAction::Close);
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_says_who_they_are_and_what_changed() {
        let known = ContactCard { name: "Bob".into(), address: "bob@y.org".into(), saved: false, notes: String::new(), sent: 3, last_sent: Some(1_700_000_000) };
        let mut card = Card::new(known.clone(), Vec::new());
        assert!(card.standing().starts_with("Suggested when you write") && card.standing().contains("3 times"), "{}", card.standing());
        assert!(!card.edited());
        card.notes = "met at the fair".into();
        assert!(card.edited());
        let saved = Card::new(ContactCard { saved: true, sent: 0, last_sent: None, ..known }, Vec::new());
        assert_eq!(saved.standing(), "In your contacts  ·  You have not written to them");
        let stranger = Card::new(ContactCard { name: "Wise".into(), address: "noreply@wise.com".into(), ..Default::default() }, Vec::new());
        assert_eq!(stranger.standing(), "Not in your contacts  ·  You have not written to them", "only writes to the user: not suggested");
    }

    #[test]
    fn the_card_lays_out_with_its_conversations() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let message = beavermail_core::MessageSummary {
            id: beavermail_core::MessageId(1),
            from: "Bob".into(),
            address: "bob@y.org".into(),
            subject: "Lunch".into(),
            preview: String::new(),
            received: "3 Oct".into(),
            timestamp: 0,
            unread: false,
            has_attachments: false,
            message_id: String::new(),
            parents: Vec::new(),
        };
        let hit = Hit { message, folder: "INBOX".into() };
        let mut card = Card::new(ContactCard { name: "Bob".into(), address: "bob@y.org".into(), ..Default::default() }, vec![(1, hit)]);
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 900.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                assert!(show(ui.ctx(), &Tokens::dark(), &mut card).is_none(), "nothing pressed");
            });
            output.textures_delta.clear();
        }
        let shown = ctx.memory(|m| m.area_rect(egui::Id::new("contact_card"))).expect("the card is shown");
        assert!(shown.width() < 600.0 && shown.height() < 800.0, "{shown:?}");
    }
}
