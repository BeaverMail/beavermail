// SPDX-License-Identifier: AGPL-3.0-only

//! A person's card: who they are to the user (in the address book, or only known from mail), their
//! name and notes, and the latest conversations with them.

use beavermail_carddav::vcard::{Address, Details, Label};
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
    /// The card of a synced address book they are, when they are one.
    pub book_card: Option<i64>,
    /// Their card is in an address book the server lets nobody change.
    pub read_only: bool,
    /// The address book their card is in, and the others that have them too.
    pub book: String,
    pub also_in: Vec<String>,
    /// All the address book has of them; `editing`, the same being changed after Edit.
    pub details: Option<Details>,
    pub editing: Option<Details>,
    /// The photo shown, by a hash of its bytes.
    photo: Option<(u64, egui::TextureHandle)>,
    /// Not in an address book, and there is one to add them to: shown as "Add to address book".
    pub can_add: bool,
}

pub enum CardAction {
    /// Save: the changes after Edit for a card of an address book, else the name and notes, on this
    /// computer only.
    Save,
    /// Choose a picture for the card being edited.
    PickPhoto,
    /// Put them in the default address book, and so on every device that syncs it.
    AddToBook,
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
        Self { name: data.name.clone(), notes: data.notes.clone(), data, recent, book_card: None, read_only: false, can_add: false, book: String::new(), also_in: Vec::new(), details: None, editing: None, photo: None }
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
        if self.book_card.is_some() {
            let mut book = match (self.read_only, self.book.is_empty()) {
                (true, _) => format!("In {}, read-only on the server", self.book),
                (false, true) => "In your address book, synced with your other devices".to_owned(),
                (false, false) => format!("In {}, synced with your other devices", self.book),
            };
            if !self.also_in.is_empty() {
                book = format!("{book}  ·  Also in {}", self.also_in.join(", "));
            }
            return format!("{book}  ·  {written}");
        }
        match (self.data.saved, self.data.sent) {
            (true, _) => format!("In your contacts on this computer  ·  {written}"),
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
        // The whole form of an address book's card is taller than a small window: it scrolls, and the
        // buttons stay below it, always in reach.
        egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 220.0).max(160.0)).show(ui, |ui| {
            action = content(ui, t, card);
        });
        action = actions(ui, t, card, true).or(action.take());
    });
    if modal.should_close() && action.is_none() {
        action = Some(CardAction::Close);
    }
    action
}

/// A picture made fit for a card: the middle square, 256 pixels wide at most, as JPEG (servers refuse
/// large cards, and phones show it small).
pub fn shrink(file: &[u8]) -> Result<Vec<u8>, String> {
    let picture = image::load_from_memory(file).map_err(|_| "This file is not a JPEG or PNG picture.".to_owned())?;
    let side = picture.width().min(picture.height());
    let square = picture.crop_imm((picture.width() - side) / 2, (picture.height() - side) / 2, side, side);
    let small = square.resize_exact(side.min(256), side.min(256), image::imageops::FilterType::Lanczos3);
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(small.to_rgb8()).write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)).map_err(|e| format!("The picture could not be made smaller: {e}"))?;
    Ok(out)
}

/// The avatar: their photo when the card has one, else their initial.
fn avatar(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, shown: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let bytes = card.editing.as_ref().or(card.details.as_ref()).and_then(|d| d.photo.clone());
    let Some(bytes) = bytes else {
        views::avatar(ui, t, shown, rect.center(), size);
        return;
    };
    let key = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    };
    if card.photo.as_ref().is_none_or(|(k, _)| *k != key) {
        card.photo = texture(ui.ctx(), &bytes, 256).map(|texture| (key, texture));
    }
    match &card.photo {
        Some((_, texture)) => {
            egui::Image::from_texture(texture).fit_to_exact_size(vec2(size, size)).corner_radius(size / 2.0).paint_at(ui, rect);
        }
        None => views::avatar(ui, t, shown, rect.center(), size),
    }
}

/// A photo ready to draw, at most `side` pixels wide; `None` for one that does not decode (shown as
/// the initial, as someone without a photo).
pub fn texture(ctx: &egui::Context, bytes: &[u8], side: u32) -> Option<egui::TextureHandle> {
    let picture = image::load_from_memory(bytes).ok()?.thumbnail(side, side).to_rgba8();
    let image = egui::ColorImage::from_rgba_unmultiplied([picture.width() as usize, picture.height() as usize], picture.as_raw());
    Some(ctx.load_texture("contact_photo", image, egui::TextureOptions::LINEAR))
}

/// What a label is called, for an email address or (`phone`) a number.
fn label_name(label: Label, phone: bool) -> &'static str {
    match label {
        Label::Work => "Work",
        Label::Home => "Home",
        Label::Mobile if phone => "Mobile",
        _ => "Other",
    }
}

/// A birthday as vCard writes it: `1815-12-10`, `18151210`, or `--12-10` without the year.
fn valid_birthday(text: &str) -> bool {
    let text = text.trim();
    let date = |t: &str, f: &str| chrono::NaiveDate::parse_from_str(t, f).is_ok();
    text.is_empty() || date(text, "%Y-%m-%d") || date(text, "%Y%m%d") || text.strip_prefix("--").is_some_and(|d| date(&format!("2000-{d}"), "%Y-%m-%d"))
}

/// Why the details being edited cannot be saved, if they cannot.
fn invalid(d: &Details) -> Option<&'static str> {
    if d.given.trim().is_empty() && d.family.trim().is_empty() && d.organization.trim().is_empty() {
        return Some("Give a name or a company");
    }
    if d.emails.iter().any(|(_, e)| !e.trim().is_empty() && !beavermail_smtp::valid_address(e.trim())) {
        return Some("An email address is not valid");
    }
    (!valid_birthday(&d.birthday)).then_some("The birthday is not a date")
}

/// A list of labelled values being edited: each row its label, its value and a button to take it out,
/// then "Add …".
fn labelled_rows(ui: &mut egui::Ui, t: &Tokens, id: &str, rows: &mut Vec<(Label, String)>, phone: bool, hint: &str, add: &str) {
    let labels: &[Label] = if phone { &[Label::Mobile, Label::Work, Label::Home, Label::Other] } else { &[Label::Work, Label::Home, Label::Other] };
    let mut gone = None;
    for (i, (label, value)) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt((id, i)).width(96.0).selected_text(label_name(*label, phone)).show_ui(ui, |ui| {
                for l in labels {
                    ui.selectable_value(label, *l, label_name(*l, phone));
                }
            });
            let width = ui.available_width() - crate::theme::CONTROL_H - ui.spacing().item_spacing.x;
            ui.add(crate::sized_field(value, width).hint_text(hint));
            if crate::icons::button(ui, t, crate::icons::Icon::X, "Remove", true, false).clicked() {
                gone = Some(i);
            }
        });
    }
    if let Some(i) = gone {
        rows.remove(i);
    }
    if ui.add(egui::Button::image_and_text(crate::icons::Icon::Plus.image(crate::icons::SIZE, t.text_dim), add).frame(false)).clicked() {
        rows.push((if phone { Label::Mobile } else { Label::Work }, String::new()));
    }
}

/// The form for everything the address book keeps of them.
fn edit_form(ui: &mut egui::Ui, t: &Tokens, d: &mut Details) -> Option<CardAction> {
    let mut action = None;
    crate::form_grid(ui, "card_details", |ui| {
        for (label, value, hint) in [("First name", &mut d.given, ""), ("Last name", &mut d.family, ""), ("Company", &mut d.organization, ""), ("Job title", &mut d.title, "")] {
            ui.label(label);
            ui.add(crate::field(value).hint_text(hint));
            ui.end_row();
        }
        ui.label("Birthday");
        ui.add(crate::field(&mut d.birthday).hint_text("1990-12-31"));
        ui.end_row();
    });
    if !valid_birthday(&d.birthday) {
        ui.colored_label(t.danger, "Write the birthday as year-month-day, such as 1990-12-31.");
    }
    ui.horizontal(|ui| {
        if ui.button(if d.photo.is_some() { "Change photo…" } else { "Add photo…" }).clicked() {
            action = Some(CardAction::PickPhoto);
        }
        if d.photo.is_some() && ui.button("Remove photo").clicked() {
            d.photo = None;
        }
    });
    let section = |ui: &mut egui::Ui, title: &str| {
        ui.add_space(10.0);
        ui.label(RichText::new(title).family(theme::bold()));
    };
    section(ui, "Email");
    labelled_rows(ui, t, "card_emails", &mut d.emails, false, "name@example.com", "Add email");
    if d.emails.iter().any(|(_, e)| !e.trim().is_empty() && !beavermail_smtp::valid_address(e.trim())) {
        ui.colored_label(t.danger, "An email address is not valid.");
    }
    section(ui, "Phone");
    labelled_rows(ui, t, "card_phones", &mut d.phones, true, "+39 02 1234 5678", "Add phone");
    section(ui, "Address");
    let mut gone = None;
    for (i, (label, a)) in d.addresses.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("card_address", i)).width(96.0).selected_text(label_name(*label, false)).show_ui(ui, |ui| {
                for l in [Label::Home, Label::Work, Label::Other] {
                    ui.selectable_value(label, l, label_name(l, false));
                }
            });
            let width = ui.available_width() - crate::theme::CONTROL_H - ui.spacing().item_spacing.x;
            ui.add(crate::sized_field(&mut a.street, width).hint_text("Street"));
            if crate::icons::button(ui, t, crate::icons::Icon::X, "Remove", true, false).clicked() {
                gone = Some(i);
            }
        });
        for pair in [[(&mut a.city, "City"), (&mut a.code, "Postcode")], [(&mut a.region, "Region"), (&mut a.country, "Country")]] {
            ui.horizontal(|ui| {
                let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                for (value, hint) in pair {
                    ui.add(crate::sized_field(value, half).hint_text(hint));
                }
            });
        }
    }
    if let Some(i) = gone {
        d.addresses.remove(i);
    }
    if ui.add(egui::Button::image_and_text(crate::icons::Icon::Plus.image(crate::icons::SIZE, t.text_dim), "Add address").frame(false)).clicked() {
        d.addresses.push((Label::Home, Address::default()));
    }
    section(ui, "Website");
    let mut gone = None;
    for (i, url) in d.urls.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let width = ui.available_width() - crate::theme::CONTROL_H - ui.spacing().item_spacing.x;
            ui.add(crate::sized_field(url, width).hint_text("https://example.com"));
            if crate::icons::button(ui, t, crate::icons::Icon::X, "Remove", true, false).clicked() {
                gone = Some(i);
            }
        });
    }
    if let Some(i) = gone {
        d.urls.remove(i);
    }
    if ui.add(egui::Button::image_and_text(crate::icons::Icon::Plus.image(crate::icons::SIZE, t.text_dim), "Add website").frame(false)).clicked() {
        d.urls.push(String::new());
    }
    section(ui, "Notes");
    ui.add(egui::TextEdit::multiline(&mut d.note).id_salt("card_details_notes").desired_rows(3).desired_width(f32::INFINITY).margin(egui::Margin::same(12)));
    action
}

/// What the address book has of them, read-only: a dim label and the value on each line.
fn detail_rows(ui: &mut egui::Ui, t: &Tokens, d: &Details, first_email: &str) {
    let mut rows: Vec<(String, String)> = Vec::new();
    rows.extend(d.emails.iter().filter(|(_, e)| !e.eq_ignore_ascii_case(first_email)).map(|(l, e)| (format!("{} email", label_name(*l, false)), e.clone())));
    rows.extend(d.phones.iter().map(|(l, p)| (label_name(*l, true).to_owned(), p.clone())));
    for (l, a) in &d.addresses {
        let place = [&a.street, &a.code, &a.city, &a.region, &a.country].iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        rows.push((format!("{} address", label_name(*l, false)), place));
    }
    let work = [d.title.trim(), d.organization.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
    if !work.is_empty() {
        rows.push(("Work".to_owned(), work));
    }
    rows.extend(d.urls.iter().map(|u| ("Website".to_owned(), u.clone())));
    if !d.birthday.is_empty() {
        rows.push(("Birthday".to_owned(), d.birthday.clone()));
    }
    if rows.is_empty() {
        return;
    }
    ui.add_space(8.0);
    egui::Grid::new("card_detail_rows").num_columns(2).spacing(vec2(16.0, 6.0)).show(ui, |ui| {
        for (label, value) in rows {
            ui.label(RichText::new(label).size(13.0).color(t.text_dim));
            ui.add(egui::Label::new(RichText::new(value).color(t.text)).wrap());
            ui.end_row();
        }
    });
}

/// The card's content, in a dialog (`closable`, with Close) or in the Contacts section.
pub fn body(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, closable: bool) -> Option<CardAction> {
    let action = content(ui, t, card);
    actions(ui, t, card, closable).or(action)
}

/// Who they are, the form when editing, and the latest conversations.
fn content(ui: &mut egui::Ui, t: &Tokens, card: &mut Card) -> Option<CardAction> {
    let mut action = None;
    let shown = if card.data.name.is_empty() { card.data.address.clone() } else { card.data.name.clone() };
    ui.horizontal(|ui| {
        avatar(ui, t, card, &shown, views::AVATAR);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(&shown).heading());
            if !card.data.name.is_empty() && !card.data.address.is_empty() {
                ui.label(RichText::new(&card.data.address).color(t.text_dim));
            }
        });
    });
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(card.standing()).size(13.0).color(t.text_dim)).wrap());
    // A card of an address book being edited: the whole form, then (in `actions`) Save and Cancel.
    if let Some(editing) = card.editing.as_mut() {
        ui.add_space(10.0);
        return edit_form(ui, t, editing);
    }
    match &card.details {
        Some(details) => detail_rows(ui, t, details, &card.data.address),
        None => {
            // A contact on this computer: its name and notes, edited in place.
            ui.add_space(10.0);
            crate::form_grid(ui, "card_fields", |ui| {
                ui.label("Name");
                ui.add(crate::field(&mut card.name).hint_text("As you call them"));
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.label("Notes");
            ui.add(egui::TextEdit::multiline(&mut card.notes).id_salt("card_notes").desired_rows(3).desired_width(f32::INFINITY).margin(egui::Margin::same(12)).hint_text("Only on this computer"));
        }
    }
    if let Some(note) = card.details.as_ref().map(|d| d.note.trim()).filter(|n| !n.is_empty()) {
        ui.add_space(8.0);
        ui.label(RichText::new("Notes").size(13.0).color(t.text_dim));
        ui.add(egui::Label::new(note).wrap());
    }
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
    action
}

/// The card's buttons: what to do with them, then Edit or Save and Close; Save and Cancel when editing.
fn actions(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, closable: bool) -> Option<CardAction> {
    let mut action = None;
    if let Some(editing) = card.editing.as_ref() {
        let why = invalid(editing);
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(4.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let changed = card.details.as_ref() != card.editing.as_ref();
            let save = views::primary_button(ui, t, "Save", why.is_none() && changed).on_disabled_hover_text(why.unwrap_or("Nothing changed yet"));
            if save.clicked() {
                action = Some(CardAction::Save);
            }
            if ui.button("Cancel").clicked() {
                card.editing = None;
            }
        });
        return action;
    }
    ui.add_space(10.0);
    // What to do with them on a row of its own: with Save and Close beside, four buttons do not fit.
    ui.horizontal(|ui| {
        // Someone the address book knows only by phone has no address to write to.
        let reachable = !card.data.address.is_empty();
        if ui.add_enabled(reachable, egui::Button::new("Write message")).on_disabled_hover_text("No email address for them").clicked() {
            action = Some(CardAction::Write);
        }
        if reachable && ui.button("Copy address").clicked() {
            ui.ctx().copy_text(card.data.address.clone());
        }
        if card.can_add && reachable && ui.button("Add to address book").on_hover_text("On the server, and every device that syncs the address book").clicked() {
            action = Some(CardAction::AddToBook);
        }
        let tip = if card.book_card.is_some() { "Delete them from your address book, on every device that syncs it" } else { "Take them out of your contacts; they stay a suggestion if you wrote to them" };
        if card.data.saved && !card.read_only && ui.button(RichText::new("Remove").color(t.danger)).on_hover_text(tip).clicked() {
            action = Some(CardAction::Remove);
        }
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
        if card.details.is_some() {
            let edit = views::primary_button(ui, t, "Edit", !card.read_only).on_disabled_hover_text("This address book is read-only on the server");
            if edit.clicked() {
                card.editing = card.details.clone();
            }
        } else {
            let (label, enabled) = if card.data.saved { ("Save", card.edited()) } else { ("Save to contacts", true) };
            if views::primary_button(ui, t, label, enabled).on_hover_text("Kept on this computer only").clicked() {
                action = Some(CardAction::Save);
            }
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
        assert_eq!(saved.standing(), "In your contacts on this computer  ·  You have not written to them");
        let stranger = Card::new(ContactCard { name: "Wise".into(), address: "noreply@wise.com".into(), ..Default::default() }, Vec::new());
        assert_eq!(stranger.standing(), "Not in your contacts  ·  You have not written to them", "only writes to the user: not suggested");
    }

    #[test]
    fn a_picture_is_made_a_small_square_and_bad_input_is_named() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(800, 600).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let small = image::load_from_memory(&shrink(&png).unwrap()).unwrap();
        assert_eq!((small.width(), small.height()), (256, 256));
        assert!(shrink(b"not a picture").is_err());
        assert!(["", "1815-12-10", "18151210", "--12-10"].iter().all(|d| valid_birthday(d)));
        assert!(["10/12/1815", "1815-13-01", "soon"].iter().all(|d| !valid_birthday(d)));
        let named = Details { given: "Ada".into(), ..Default::default() };
        assert_eq!(invalid(&named), None);
        assert_eq!(invalid(&Details::default()), Some("Give a name or a company"));
        assert_eq!(invalid(&Details { emails: vec![(Label::Work, "ada@".into())], ..named }), Some("An email address is not valid"));
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
        // An address book's card being edited, with a bit of everything: it scrolls within the window.
        let details = Details { given: "Bob".into(), emails: vec![(Label::Work, "bob@y.org".into())], phones: vec![(Label::Mobile, "+39 02 1234".into())], addresses: vec![(Label::Home, Address { city: "Milano".into(), ..Default::default() })], urls: vec!["https://bob.example".into()], ..Default::default() };
        (card.book_card, card.details, card.editing) = (Some(1), Some(details.clone()), Some(details));
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 700.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                assert!(show(ui.ctx(), &Tokens::dark(), &mut card).is_none(), "nothing pressed");
            });
            output.textures_delta.clear();
        }
        let shown = ctx.memory(|m| m.area_rect(egui::Id::new("contact_card"))).unwrap();
        assert!(shown.height() <= 700.0, "{shown:?}");
    }
}
