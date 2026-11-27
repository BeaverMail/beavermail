// SPDX-License-Identifier: AGPL-3.0-only

//! A person's card: who they are to the user (in the address book, or only known from mail), their
//! name and notes, and the latest conversations with them.

use beavermail_carddav::vcard::{Address, Details, Label};
use beavermail_core::Hit;
use beavermail_store::ContactCard;
use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::icons::{self, Icon};
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
    /// Leaving the edit with changes, asked first: what to do once they are discarded.
    pub asking: Option<Leave>,
    /// Save was pressed with something wrong: every problem is said, in the danger colour.
    tried: bool,
    /// The form scrolls to the first problem, once, after that Save.
    show_problem: bool,
    /// The photo shown, by a hash of its bytes.
    photo: Option<(u64, egui::TextureHandle)>,
    /// Not in an address book, and there is one to add them to: shown as "Add to address book".
    pub can_add: bool,
    /// The card is in the address book on this computer, never synced.
    pub local: bool,
    /// The other address books it can go to, by URL with their names.
    pub targets: Vec<(String, String)>,
}

/// A way out of editing a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// Back to the card as it was.
    Cancel,
    /// The card goes too.
    Close,
    /// Another card of the address book opens in its place.
    Open(i64),
    /// The card makes way for a new contact.
    NewContact,
    /// The app is closing.
    Quit,
}

pub enum CardAction {
    /// The edit was left, its changes discarded: go on with what left it (never [`Leave::Cancel`],
    /// which the card does itself).
    Left(Leave),
    /// Save the changes, then close the app.
    SaveAndQuit,
    /// Save: the changes after Edit for a card of an address book, else the name and notes, on this
    /// computer only.
    Save,
    /// Choose a picture for the card being edited.
    PickPhoto,
    /// Put them in the default address book, and so on every device that syncs it.
    AddToBook,
    /// Take them out of the address book.
    Remove,
    /// Save the card to a vCard file.
    Export,
    /// Put it in this other address book (by URL), out of its own.
    MoveTo(String),
    /// Put a copy of it in this other address book (by URL).
    CopyTo(String),
    /// Write them a new message.
    Write,
    /// Open this message of the conversations with them, by its place in `recent`.
    Open(usize),
    Close,
}

impl Card {
    pub fn new(data: ContactCard, recent: Vec<(u64, Hit)>) -> Self {
        Self { name: data.name.clone(), notes: data.notes.clone(), data, recent, book_card: None, read_only: false, can_add: false, local: false, targets: Vec::new(), asking: None, tried: false, show_problem: false, book: String::new(), also_in: Vec::new(), details: None, editing: None, photo: None }
    }

    /// Edited, and changed from what is saved.
    fn changed(&self) -> bool {
        self.editing.is_some() && self.editing != self.details
    }

    /// Leaves the edit `how`: at once when nothing changed (true, the caller goes on), else after
    /// asking whether to discard the changes (false; the answer comes as [`CardAction::Left`]).
    pub fn leave(&mut self, how: Leave) -> bool {
        if self.changed() {
            self.asking = Some(how);
            return false;
        }
        self.editing = None;
        true
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
                _ if self.local => "In your contacts on this computer".to_owned(),
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
    let modal = crate::views::modal(ctx, egui::Id::new("contact_card")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        // The whole form of an address book's card is taller than a small window: it scrolls, and the
        // buttons stay below it, always in reach.
        egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 220.0).max(160.0)).show(ui, |ui| {
            action = content(ui, t, card, true);
        });
        action = actions(ui, t, card).or(action.take());
    });
    let asked = discard_dialog(ctx, t, card);
    // Esc while editing leaves the edit, not the card.
    if modal.should_close() && action.is_none() && card.asking.is_none() {
        let how = if card.editing.is_some() { Leave::Cancel } else { Leave::Close };
        if card.leave(how) && how == Leave::Close {
            action = Some(CardAction::Close);
        }
    }
    asked.or(action)
}

/// "Discard changes?" when an edit with changes is left; Discard goes on as it was left.
fn discard_dialog(ctx: &egui::Context, t: &Tokens, card: &mut Card) -> Option<CardAction> {
    let how = card.asking?;
    let who = if card.data.name.is_empty() { card.data.address.clone() } else { card.data.name.clone() };
    let quitting = how == Leave::Quit;
    // Closing the app, the changes can be saved on the way out when they can be saved at all.
    let savable = quitting && card.editing.as_ref().is_some_and(|e| invalid(e).is_none());
    let (mut discard, mut keep, mut save) = (false, false, false);
    let modal = crate::views::modal(ctx, egui::Id::new("discard_card")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH * 0.8);
        let heading = match (quitting, who.is_empty()) {
            (true, true) => "Quit with your changes not saved?".to_owned(),
            (true, false) => format!("Quit with the changes to {who} not saved?"),
            (false, true) => "Discard your changes?".to_owned(),
            (false, false) => format!("Discard changes to {who}?"),
        };
        ui.heading(heading);
        ui.label(RichText::new("What you changed on this card will be lost.").color(t.text_dim));
        ui.add_space(12.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            discard = ui.add(egui::Button::new(RichText::new(if quitting { "Discard and quit" } else { "Discard" }).color(t.on_accent)).fill(t.danger)).clicked();
            save = savable && ui.button("Save and quit").clicked();
            keep = ui.button("Keep editing").clicked();
        });
    });
    if save {
        card.asking = None;
        return Some(CardAction::SaveAndQuit);
    }
    if keep || modal.should_close() {
        card.asking = None;
        return None;
    }
    if !discard {
        return None;
    }
    card.asking = None;
    card.editing = None;
    (how != Leave::Cancel).then_some(CardAction::Left(how))
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
/// While editing it is where the photo is changed, as in most contact apps: a camera badge, a click
/// to choose one, or with a photo already, a menu to change or remove it.
fn avatar(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, shown: &str, size: f32) -> Option<CardAction> {
    let editing = card.editing.is_some();
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), if editing { egui::Sense::click() } else { egui::Sense::hover() });
    picture(ui, t, card, shown, rect);
    if !editing {
        return None;
    }
    let has_photo = card.editing.as_ref().is_some_and(|d| d.photo.is_some());
    let label = if has_photo { "Change or remove the photo" } else { "Add a photo" };
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let painter = ui.painter();
    if response.hovered() {
        painter.circle_filled(rect.center(), size / 2.0, t.bg.gamma_multiply(0.6));
        Icon::Camera.image(icons::SIZE, t.text).paint_at(ui, egui::Rect::from_center_size(rect.center(), vec2(icons::SIZE, icons::SIZE)));
    }
    // The badge says it can be changed before the pointer finds it.
    let badge = egui::Rect::from_center_size(rect.right_bottom() - vec2(5.0, 5.0), vec2(20.0, 20.0));
    painter.circle_filled(badge.center(), 10.0, t.surface);
    painter.circle_stroke(badge.center(), 10.0, egui::Stroke::new(1.0, t.border));
    Icon::Camera.image(12.0, t.text).paint_at(ui, egui::Rect::from_center_size(badge.center(), vec2(12.0, 12.0)));
    let mut action = None;
    if !has_photo {
        if response.clicked() {
            action = Some(CardAction::PickPhoto);
        }
        return action;
    }
    egui::Popup::menu(&response).style(views::menu_look).show(|ui| {
        if ui.button("Change photo…").clicked() {
            action = Some(CardAction::PickPhoto);
        }
        if ui.button(RichText::new("Remove photo").color(t.danger)).clicked()
            && let Some(d) = card.editing.as_mut()
        {
            d.photo = None;
        }
    });
    action
}

/// The photo when the card has one, else their initial.
fn picture(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, shown: &str, rect: egui::Rect) {
    let size = rect.width();
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
    text.trim().is_empty() || Birthday::parse(text).complete()
}

/// A birthday in its parts, as the form picks them: the year can be left out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Birthday {
    day: Option<u32>,
    month: Option<u32>,
    year: Option<i32>,
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

impl Birthday {
    /// From `1815-12-10`, `18151210`, `--12-10` or `--1210`; nothing from anything else.
    fn parse(text: &str) -> Self {
        use chrono::Datelike;
        let text = text.trim();
        let date = |t: &str, f: &str| chrono::NaiveDate::parse_from_str(t, f).ok();
        if let Some(d) = date(text, "%Y-%m-%d").or_else(|| date(text, "%Y%m%d")) {
            return Self { day: Some(d.day()), month: Some(d.month()), year: Some(d.year()) };
        }
        // 2000 is a leap year: the 29th of February is a birthday without a year too.
        let no_year = text.strip_prefix("--").and_then(|d| date(&format!("2000-{d}"), "%Y-%m-%d").or_else(|| date(&format!("2000{d}"), "%Y%m%d")));
        no_year.map_or_else(Self::default, |d| Self { day: Some(d.day()), month: Some(d.month()), year: None })
    }

    fn complete(self) -> bool {
        self.day.is_some() && self.month.is_some() && self.day <= Some(self.days())
    }

    /// As vCard writes it; empty when nothing is picked, and not a date while day or month is missing.
    fn text(self) -> String {
        match (self.day, self.month, self.year) {
            (None, None, None) => String::new(),
            (Some(d), Some(m), y) if self.complete() => y.map_or_else(|| format!("--{m:02}-{d:02}"), |y| format!("{y:04}-{m:02}-{d:02}")),
            _ => "incomplete".to_owned(),
        }
    }

    /// The days of its month: 31 while none is picked, the 29th of February without a year.
    fn days(self) -> u32 {
        let Some(month) = self.month else { return 31 };
        let first = chrono::NaiveDate::from_ymd_opt(self.year.unwrap_or(2000), month, 1);
        first.and_then(|f| f.checked_add_months(chrono::Months::new(1))).zip(first).map_or(31, |(next, first)| (next - first).num_days() as u32)
    }

    /// "10 December 1990 · 35 years old" on `today`, "10 December" without the year.
    fn shown(self, today: chrono::NaiveDate) -> Option<String> {
        use chrono::Datelike;
        let (day, month) = (self.day?, self.month?);
        let name = MONTHS[month as usize - 1];
        Some(match self.year {
            None => format!("{day} {name}"),
            Some(year) => {
                let age = today.year() - year - i32::from((today.month(), today.day()) < (month, day));
                if age >= 0 { format!("{day} {name} {year}  ·  {age} years old") } else { format!("{day} {name} {year}") }
            }
        })
    }
}

/// A grid of choices drawn as the calendar's days: the one picked filled in the accent; returns the
/// place of the one clicked.
fn choice_grid(ui: &mut egui::Ui, t: &Tokens, labels: &[String], picked: Option<usize>, columns: usize, cell: egui::Vec2, open: impl Fn(usize) -> bool) -> Option<usize> {
    let mut clicked = None;
    ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
    ui.spacing_mut().interact_size.y = 0.0;
    for row in labels.chunks(columns).enumerate() {
        ui.horizontal(|ui| {
            for (col, label) in row.1.iter().enumerate() {
                let i = row.0 * columns + col;
                let enabled = open(i);
                let (rect, response) = ui.allocate_exact_size(cell, if enabled { egui::Sense::click() } else { egui::Sense::hover() });
                let spot = rect.shrink2(vec2(3.0, 2.0));
                let chosen = picked == Some(i);
                if chosen {
                    ui.painter().rect_filled(spot, egui::CornerRadius::same(theme::CONTROL_RADIUS), t.accent);
                } else if enabled && response.hovered() {
                    ui.painter().rect_filled(spot, egui::CornerRadius::same(theme::CONTROL_RADIUS), t.hover);
                }
                let color = if chosen { t.on_accent } else if enabled { t.text } else { t.text_dim.gamma_multiply(0.5) };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(14.0), color);
                if enabled {
                    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, chosen, label));
                    if response.clicked() {
                        clicked = Some(i);
                    }
                }
            }
        });
    }
    clicked
}

/// The birthday as day, month and year, each picked: the day from the days of the month, the year
/// from a decade, or none. While day or month is missing, `text` is not a date and Save waits; the
/// help returned says what is still to pick.
fn birthday_field(ui: &mut egui::Ui, t: &Tokens, text: &mut String) -> Option<&'static str> {
    use chrono::Datelike;
    let id = ui.id().with("birthday");
    // The parts picked so far, kept while they make no date yet; read again when the text changed elsewhere.
    let mut parts = ui.data(|m| m.get_temp::<(Birthday, String)>(id)).filter(|(_, seen)| seen == text).map_or_else(|| Birthday::parse(text), |(parts, _)| parts);
    let this_year = chrono::Local::now().year();
    let placeholder = |s: &str| RichText::new(s).color(t.text_dim);
    ui.horizontal(|ui| {
        let day = ui.add_sized([56.0, theme::CONTROL_H], egui::Button::new(parts.day.map_or_else(|| placeholder("Day"), |d| RichText::new(d.to_string()))));
        egui::Popup::from_toggle_button_response(&day).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            let labels: Vec<String> = (1..=parts.days()).map(|d| d.to_string()).collect();
            if let Some(i) = choice_grid(ui, t, &labels, parts.day.map(|d| d as usize - 1), 7, vec2(34.0, 30.0), |_| true) {
                parts.day = Some(i as u32 + 1);
                ui.close();
            }
        });
        egui::ComboBox::from_id_salt(id.with("month")).width(112.0).height(400.0).selected_text(parts.month.map_or_else(|| placeholder("Month"), |m| RichText::new(MONTHS[m as usize - 1]))).show_ui(ui, |ui| {
            for (i, name) in MONTHS.iter().enumerate() {
                ui.selectable_value(&mut parts.month, Some(i as u32 + 1), *name);
            }
        });
        let year = ui.add_sized([72.0, theme::CONTROL_H], egui::Button::new(parts.year.map_or_else(|| placeholder("Year"), |y| RichText::new(y.to_string())))).on_hover_text("The year can be left out");
        let decade_id = id.with("decade");
        if year.clicked() {
            let start = parts.year.unwrap_or(this_year - 30);
            ui.data_mut(|m| m.insert_temp(decade_id, start - start.rem_euclid(10)));
        }
        egui::Popup::from_toggle_button_response(&year).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            let mut decade: i32 = ui.data(|m| m.get_temp(decade_id)).unwrap_or(this_year - this_year.rem_euclid(10));
            let cell = vec2(56.0, 30.0);
            ui.allocate_ui_with_layout(vec2(cell.x * 5.0, cell.y), Layout::left_to_right(Align::Center), |ui| {
                let arrow = |ui: &mut egui::Ui, text: &str, enabled: bool| ui.add_enabled(enabled, egui::Button::new(RichText::new(text).size(18.0)).frame(false).min_size(vec2(34.0, cell.y))).clicked();
                if arrow(ui, "‹", decade > 1900) {
                    decade -= 10;
                }
                ui.allocate_ui_with_layout(vec2(cell.x * 5.0 - 68.0, cell.y), Layout::centered_and_justified(egui::Direction::LeftToRight), |ui| {
                    ui.label(RichText::new(format!("{decade} – {}", decade + 9)).family(theme::bold()));
                });
                if arrow(ui, "›", decade + 10 <= this_year) {
                    decade += 10;
                }
            });
            ui.data_mut(|m| m.insert_temp(decade_id, decade));
            let labels: Vec<String> = (decade..decade + 10).map(|y| y.to_string()).collect();
            let picked = parts.year.and_then(|y| usize::try_from(y - decade).ok()).filter(|i| *i < 10);
            if let Some(i) = choice_grid(ui, t, &labels, picked, 5, cell, |i| decade + i as i32 <= this_year) {
                parts.year = Some(decade + i as i32);
                ui.close();
            }
            if parts.year.is_some() && ui.add(egui::Button::new(placeholder("No year")).frame(false)).clicked() {
                parts.year = None;
                ui.close();
            }
        });
        if parts != Birthday::default() && icons::button(ui, t, Icon::X, "No birthday", true, false).clicked() {
            parts = Birthday::default();
        }
    });
    // A month with fewer days takes the day down to its last.
    if let Some(day) = parts.day {
        parts.day = Some(day.min(parts.days()));
    }
    *text = parts.text();
    ui.data_mut(|m| m.insert_temp(id, (parts, text.clone())));
    match (parts.day, parts.month) {
        _ if parts == Birthday::default() || parts.complete() => None,
        (Some(_), None) => Some("Pick the month too"),
        (None, Some(_)) => Some("Pick the day too"),
        _ => Some("Pick the day and the month too"),
    }
}

/// Why the details being edited cannot be saved, if they cannot.
fn invalid(d: &Details) -> Option<&'static str> {
    if d.given.trim().is_empty() && d.family.trim().is_empty() && d.organization.trim().is_empty() {
        return Some("Give a name or a company");
    }
    if d.emails.iter().any(|(_, e)| !e.trim().is_empty() && !beavermail_smtp::valid_address(e.trim())) {
        return Some("An email address is not valid");
    }
    (!valid_birthday(&d.birthday)).then_some("Pick the day and the month of the birthday")
}

/// A list of labelled values being edited: each row its label, its value and a button to take it out,
/// then "Add …".
/// `wrong`: what is wrong with a value, said under its row once it is left.
#[allow(clippy::too_many_arguments)]
fn labelled_rows(ui: &mut egui::Ui, t: &Tokens, id: &str, rows: &mut Vec<(Label, String)>, phone: bool, hint: &str, add: &str, show_problem: &mut bool, wrong: impl Fn(&str) -> Option<&'static str>) {
    let labels: &[Label] = if phone { &[Label::Mobile, Label::Work, Label::Home, Label::Other] } else { &[Label::Work, Label::Home, Label::Other] };
    let mut gone = None;
    for (i, (label, value)) in rows.iter_mut().enumerate() {
        let note = ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt((id, i)).width(96.0).selected_text(label_name(*label, phone)).show_ui(ui, |ui| {
                for l in labels {
                    ui.selectable_value(label, *l, label_name(*l, phone));
                }
            });
            let width = ui.available_width() - crate::theme::CONTROL_H - ui.spacing().item_spacing.x;
            let field = ui.add(crate::sized_field(value, width).hint_text(hint));
            if crate::icons::button(ui, t, crate::icons::Icon::X, "Remove", true, false).clicked() {
                gone = Some(i);
            }
            // Said once the field is left, not while it is typed in.
            let why = wrong(value.trim()).filter(|_| !field.has_focus());
            (field.rect.left(), field.rect.width(), why)
        })
        .inner;
        if let (left, width, Some(why)) = note {
            let shown = views::field_note(ui, t, why, true, left, width);
            if std::mem::take(show_problem) {
                shown.scroll_to_me(Some(Align::Center));
            }
        }
    }
    if let Some(i) = gone {
        rows.remove(i);
    }
    if ui.add(egui::Button::image_and_text(crate::icons::Icon::Plus.image(crate::icons::SIZE, t.text_dim), add).frame(false)).clicked() {
        rows.push((if phone { Label::Mobile } else { Label::Work }, String::new()));
    }
}

/// The form for everything the address book keeps of them.
/// `tried`: Save was pressed with something wrong, so each problem is said in the danger colour;
/// `show_problem`: the first one is scrolled to.
fn edit_form(ui: &mut egui::Ui, t: &Tokens, d: &mut Details, tried: bool, show_problem: &mut bool) {
    // A note right under the fields above it (a row of the grid would be a whole control's height away).
    let mut note = |ui: &mut egui::Ui, left: f32, text: &str, error: bool| {
        let shown = views::field_note(ui, t, text, error, left, crate::FIELD_WIDTH);
        if error && std::mem::take(show_problem) {
            shown.scroll_to_me(Some(Align::Center));
        }
    };
    let mut field_left = 0.0;
    crate::form_grid(ui, "card_names", |ui| {
        for (label, value) in [("First name", &mut d.given), ("Last name", &mut d.family), ("Company", &mut d.organization)] {
            ui.label(label);
            field_left = ui.cursor().left();
            ui.add(crate::field(value));
            ui.end_row();
        }
    });
    if tried && d.given.trim().is_empty() && d.family.trim().is_empty() && d.organization.trim().is_empty() {
        note(ui, field_left, "Give a first or last name, or a company", true);
    }
    let mut help = None;
    crate::form_grid(ui, "card_details", |ui| {
        ui.label("Job title");
        ui.add(crate::field(&mut d.title));
        ui.end_row();
        ui.label("Birthday");
        help = birthday_field(ui, t, &mut d.birthday);
        ui.end_row();
    });
    // Still being picked: what is missing, quietly; after Save, as a problem.
    if let Some(help) = help {
        note(ui, field_left, help, tried);
    }
    let section = |ui: &mut egui::Ui, title: &str| {
        ui.add_space(10.0);
        ui.label(RichText::new(title).family(theme::bold()));
    };
    section(ui, "Email");
    labelled_rows(ui, t, "card_emails", &mut d.emails, false, "name@example.com", "Add email", show_problem, |e| (!e.is_empty() && !beavermail_smtp::valid_address(e)).then_some("Not an email address: write it as name@example.com"));
    section(ui, "Phone");
    labelled_rows(ui, t, "card_phones", &mut d.phones, true, "+39 02 1234 5678", "Add phone", show_problem, |_| None);
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
        let shown = Birthday::parse(&d.birthday).shown(chrono::Local::now().date_naive());
        rows.push(("Birthday".to_owned(), shown.unwrap_or_else(|| d.birthday.clone())));
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
    let action = content(ui, t, card, closable);
    let action = actions(ui, t, card).or(action);
    // Esc while editing leaves the edit; a menu open takes it first.
    if card.editing.is_some() && card.asking.is_none() && !egui::Popup::is_any_open(ui.ctx()) && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        card.leave(Leave::Cancel);
    }
    discard_dialog(ui.ctx(), t, card).or(action)
}

/// Who they are, with Edit, More and Close on the right as a message's header has its buttons; what
/// to do with them; the form when editing; and the latest conversations.
fn content(ui: &mut egui::Ui, t: &Tokens, card: &mut Card, closable: bool) -> Option<CardAction> {
    let mut action = None;
    let shown = if card.data.name.is_empty() { card.data.address.clone() } else { card.data.name.clone() };
    ui.horizontal(|ui| {
        if let Some(chosen) = avatar(ui, t, card, &shown, views::AVATAR) {
            action = Some(chosen);
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(&shown).heading());
            if !card.data.name.is_empty() && !card.data.address.is_empty() {
                ui.label(RichText::new(&card.data.address).color(t.text_dim));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if closable && icons::button(ui, t, Icon::X, "Close (Esc)", true, false).clicked() && card.leave(Leave::Close) {
                action = Some(CardAction::Close);
            }
            // While editing, Save and Cancel below are the only way out.
            if card.editing.is_none() {
                if let Some(chosen) = more_menu(ui, t, card) {
                    action = Some(chosen);
                }
                if card.details.is_some() {
                    let tip = if card.read_only { "This address book is read-only on the server" } else { "Edit" };
                    if icons::button(ui, t, Icon::Pencil, tip, !card.read_only, false).clicked() {
                        (card.editing, card.tried) = (card.details.clone(), false);
                    }
                }
            }
        });
    });
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(card.standing()).size(13.0).color(t.text_dim)).wrap());
    // A card of an address book being edited: the whole form, then (in `actions`) Save and Cancel.
    if let Some(editing) = card.editing.as_mut() {
        ui.add_space(10.0);
        edit_form(ui, t, editing, card.tried, &mut card.show_problem);
        return action;
    }
    ui.add_space(12.0);
    if let Some(chosen) = quick_actions(ui, t, card) {
        action = Some(chosen);
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

/// "Move to" (when `movable`: out of a read-only address book it can only be copied) and "Copy to",
/// each listing the other address books it can go to.
pub fn book_menus(ui: &mut egui::Ui, targets: &[(String, String)], movable: bool) -> Option<CardAction> {
    if targets.is_empty() {
        return None;
    }
    let mut action = None;
    for (label, make) in [("Move to", CardAction::MoveTo as fn(String) -> CardAction), ("Copy to", CardAction::CopyTo)] {
        if label == "Move to" && !movable {
            continue;
        }
        ui.menu_button(label, |ui| {
            for (url, name) in targets {
                if ui.button(name).clicked() {
                    action = Some(make(url.clone()));
                    ui.close();
                }
            }
        });
    }
    action
}

/// Write message, the one main action, then copying the address and adding them to an address book.
fn quick_actions(ui: &mut egui::Ui, t: &Tokens, card: &Card) -> Option<CardAction> {
    // Someone the address book knows only by phone has no address to write to: no row at all, as a
    // card offers only what its details allow.
    if card.data.address.is_empty() {
        return None;
    }
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        if views::primary_icon(ui, t, Icon::Mail, "Write message", None, 0.0).clicked() {
            action = Some(CardAction::Write);
        }
        if ui.add(egui::Button::image_and_text(Icon::Copy.image(icons::SIZE, t.text_dim), "Copy address")).clicked() {
            ui.ctx().copy_text(card.data.address.clone());
        }
        if card.can_add && ui.add(egui::Button::image_and_text(Icon::Plus.image(icons::SIZE, t.text_dim), "Add to address book")).on_hover_text("On the server, and every device that syncs the address book").clicked() {
            action = Some(CardAction::AddToBook);
        }
    });
    action
}

/// "More": what is done to the card now and then, Remove last and apart.
fn more_menu(ui: &mut egui::Ui, t: &Tokens, card: &Card) -> Option<CardAction> {
    let exportable = card.book_card.is_some() || card.data.saved;
    let removable = card.data.saved && !card.read_only;
    if !exportable && !removable {
        return None;
    }
    let mut action = None;
    let more = icons::button(ui, t, Icon::Ellipsis, "More: Export, Move to, Copy to, Remove", true, false);
    egui::Popup::menu(&more).style(views::menu_look).show(|ui| {
        if exportable && ui.button("Export…").on_hover_text("Save the card as a vCard file").clicked() {
            action = Some(CardAction::Export);
        }
        if card.book_card.is_some() && let Some(chosen) = book_menus(ui, &card.targets, !card.read_only) {
            action = Some(chosen);
        }
        if removable {
            ui.separator();
            let tip = if card.local { "Delete them from your contacts on this computer" } else { "Delete them from your address book, on every device that syncs it" };
            if ui.button(RichText::new("Remove").color(t.danger)).on_hover_text(tip).clicked() {
                action = Some(CardAction::Remove);
            }
        }
    });
    action
}

/// The bottom row, only when there is something to save: Save and Cancel when editing, Save for the
/// name and notes of a contact on this computer, Save to contacts for someone not yet saved.
fn actions(ui: &mut egui::Ui, t: &Tokens, card: &mut Card) -> Option<CardAction> {
    let mut action = None;
    if card.editing.is_none() && card.details.is_some() {
        return None;
    }
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(4.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
        if let Some(editing) = card.editing.as_ref() {
            let why = invalid(editing);
            let changed = card.details.as_ref() != card.editing.as_ref();
            // Pressed with something wrong, it says what, under each field, rather than saving.
            let save = views::primary_button(ui, t, "Save", changed).on_disabled_hover_text("Nothing changed yet");
            if save.clicked() {
                match why {
                    Some(_) => (card.tried, card.show_problem) = (true, true),
                    None => action = Some(CardAction::Save),
                }
            }
            if ui.button("Cancel").on_hover_text("Esc").clicked() {
                card.leave(Leave::Cancel);
            }
        } else {
            let (label, enabled) = if card.data.saved { ("Save", card.edited()) } else { ("Save to contacts", true) };
            if views::primary_button(ui, t, label, enabled).on_hover_text("Kept on this computer only").clicked() {
                action = Some(CardAction::Save);
            }
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_with_changes_is_left_only_after_asking() {
        let details = Details { given: "Bob".into(), ..Default::default() };
        let mut card = Card::new(ContactCard { name: "Bob".into(), ..Default::default() }, Vec::new());
        (card.book_card, card.details, card.editing) = (Some(1), Some(details.clone()), Some(details.clone()));
        assert!(card.leave(Leave::Open(2)), "nothing changed: left at once");
        assert!(card.editing.is_none() && card.asking.is_none());
        card.editing = Some(Details { given: "Robert".into(), ..details });
        assert!(!card.leave(Leave::Close), "changed: asked first");
        assert_eq!(card.asking, Some(Leave::Close));
        assert!(card.editing.is_some(), "the changes stay until discarded");
    }

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
    }

    #[test]
    fn a_birthday_is_picked_in_parts_and_shown_with_the_age() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 27).unwrap();
        let full = Birthday::parse("1990-12-10");
        assert_eq!(full, Birthday { day: Some(10), month: Some(12), year: Some(1990) });
        assert_eq!(full.text(), "1990-12-10");
        assert_eq!(full.shown(today).unwrap(), "10 December 1990  ·  35 years old", "not 36 before the day");
        let no_year = Birthday::parse("--02-29");
        assert_eq!((no_year.text().as_str(), no_year.shown(today).unwrap().as_str()), ("--02-29", "29 February"));
        assert_eq!(Birthday::parse("--0229"), no_year);
        assert_eq!(Birthday { year: Some(2023), ..no_year }.days(), 28, "no 29th in 2023");
        assert!(!Birthday { year: Some(2023), ..no_year }.complete());
        let half = Birthday { day: Some(3), ..Default::default() };
        assert!(!valid_birthday(&half.text()), "a day without a month is not a birthday yet");
        assert_eq!(Birthday::default().text(), "", "none at all is fine");
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
            pinned: false,
            starred: false,
            priority: 0,
            labels: Vec::new(),
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
