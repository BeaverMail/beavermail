//! Painted building blocks of the main window: folder rows, message rows, avatars, search box,
//! empty states. All colours come from [`Tokens`]; all sizes are named here or in `theme`.

use std::sync::Arc;

use beavermail_core::{MessageSummary, Role};
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, Rect, Response, Sense, Ui, pos2, vec2};
use egui::text::{LayoutJob, TextWrapping};

use crate::icons::{self, Icon};
use crate::theme::{self, CONTROL_H, CONTROL_RADIUS, Tokens};

/// Fixed row height, so the list can be virtualised with `show_rows`.
pub const MESSAGE_ROW_H: f32 = 84.0;
const FOLDER_ROW_H: f32 = 38.0;
pub const AVATAR: f32 = 40.0;

fn tint(t: &Tokens) -> Color32 {
    t.accent.gamma_multiply(0.16)
}

/// Single-line text that ends in an ellipsis instead of overflowing `width`.
fn truncated(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(width.max(0.0));
    ui.painter().layout_job(job)
}

fn row_background(ui: &Ui, rect: Rect, response: &Response, selected: bool, t: &Tokens) {
    let fill = if selected {
        tint(t)
    } else if response.is_pointer_button_down_on() {
        t.pressed
    } else if response.hovered() {
        t.hover
    } else {
        return;
    };
    ui.painter().rect_filled(rect, CornerRadius::same(CONTROL_RADIUS), fill);
}

/// Up to two initials of a display name.
fn initials(name: &str) -> String {
    let mut letters = name.split_whitespace().filter_map(|w| w.chars().find(|c| c.is_alphanumeric()));
    let first = letters.next();
    let last = letters.next_back();
    first.into_iter().chain(last).flat_map(char::to_uppercase).collect()
}

/// A round avatar with the person's initials, centred on `center`.
pub fn avatar(ui: &Ui, t: &Tokens, name: &str, center: egui::Pos2, diameter: f32) {
    let (bg, fg) = theme::avatar_colors(name, t.dark);
    ui.painter().circle_filled(center, diameter / 2.0, bg);
    let text = initials(name);
    let text = if text.is_empty() { "?".to_owned() } else { text };
    ui.painter().text(center, egui::Align2::CENTER_CENTER, text, FontId::proportional(diameter * 0.36), fg);
}

/// The folder name as shown: INBOX in capitals is noise, and nested names show their last part.
pub fn folder_title(name: &str, role: Role) -> String {
    if role == Role::Inbox {
        return "Inbox".to_owned();
    }
    name.rsplit(['/', '.']).next().unwrap_or(name).to_owned()
}

/// Sidebar order: the usual mail folders first, then the rest alphabetically.
pub fn folder_rank(role: Role) -> u8 {
    match role {
        Role::Inbox => 0,
        Role::Starred => 1,
        Role::Drafts => 2,
        Role::Sent => 3,
        Role::Archive => 4,
        Role::Junk => 5,
        Role::Trash => 6,
        Role::Other => 7,
    }
}

fn role_icon(role: Role) -> Icon {
    match role {
        Role::Inbox => Icon::Inbox,
        Role::Starred => Icon::Star,
        Role::Drafts => Icon::Drafts,
        Role::Sent => Icon::Send,
        Role::Archive => Icon::Archive,
        Role::Junk => Icon::Spam,
        Role::Trash => Icon::Trash,
        Role::Other => Icon::Folder,
    }
}

/// A folder in the sidebar: coloured icon, name, and the unread count in a pill.
pub fn folder_row(ui: &mut Ui, t: &Tokens, title: &str, role: Role, unread: u32, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), FOLDER_ROW_H), Sense::click());
    row_background(ui, rect, &response, selected, t);
    role_icon(role)
        .image(icons::SIZE, theme::role_color(role))
        .paint_at(ui, Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(icons::SIZE, icons::SIZE)));
    let mut right = 12.0;
    if unread > 0 {
        let galley = ui.painter().layout_no_wrap(unread.to_string(), FontId::proportional(12.5), if selected { t.accent } else { t.text_dim });
        let pill = Rect::from_center_size(
            pos2(rect.right() - 12.0 - (galley.size().x + 16.0) / 2.0, rect.center().y),
            vec2((galley.size().x + 16.0).max(26.0), 22.0),
        );
        ui.painter().rect_filled(pill, CornerRadius::same(11), if selected { t.accent.gamma_multiply(0.22) } else { t.hover });
        ui.painter().galley(pill.center() - galley.size() / 2.0, galley, t.text_dim);
        right += pill.width() + 8.0;
    }
    let color = if selected { t.accent } else { t.text };
    let galley = truncated(ui, title, FontId::proportional(15.0), color, rect.width() - 46.0 - right);
    ui.painter().galley(pos2(rect.left() + 42.0, rect.center().y - galley.size().y / 2.0), galley, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A message in the list: avatar, sender with unread dot, date, subject and a one-line preview.
pub fn message_row(ui: &mut Ui, t: &Tokens, message: &MessageSummary, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), MESSAGE_ROW_H), Sense::click());
    let card = rect.shrink2(vec2(8.0, 2.0));
    row_background(ui, card, &response, selected, t);
    avatar(ui, t, &message.from, pos2(card.left() + 14.0 + AVATAR / 2.0, card.top() + 14.0 + AVATAR / 2.0), AVATAR);
    let left = card.left() + 14.0 + AVATAR + 12.0;
    let width = card.right() - 14.0 - left;

    let date = ui.painter().layout_no_wrap(message.received.clone(), FontId::proportional(13.0), t.text_dim);
    let date_w = date.size().x;
    ui.painter().galley(pos2(card.right() - 14.0 - date_w, card.top() + 13.0), date, t.text_dim);

    let primary = if message.unread { t.text } else { t.text_dim };
    let dot = if message.unread { 16.0 } else { 0.0 };
    let sender = truncated(ui, &message.from, FontId::proportional(15.5), primary, width - date_w - 12.0 - dot);
    let sender_w = sender.size().x;
    ui.painter().galley(pos2(left, card.top() + 11.0), sender, primary);
    if message.unread {
        ui.painter().circle_filled(pos2(left + sender_w + 10.0, card.top() + 21.0), 4.0, t.accent);
    }
    let subject = truncated(ui, &message.subject, FontId::proportional(14.5), primary, width);
    ui.painter().galley(pos2(left, card.top() + 33.0), subject, primary);
    let preview = truncated(ui, &message.preview, FontId::proportional(13.5), t.text_dim, width);
    ui.painter().galley(pos2(left, card.top() + 56.0), preview, t.text_dim);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A search field with a leading icon and a clear button. `focus` moves the caret into it.
pub fn search_box(ui: &mut Ui, t: &Tokens, text: &mut String, hint: &str, width: f32, focus: bool) -> Response {
    let frame = egui::Frame::new()
        .fill(t.input)
        .stroke(egui::Stroke::new(1.0, t.border_strong))
        .corner_radius(CornerRadius::same(CONTROL_RADIUS))
        .inner_margin(Margin::symmetric(12, 0));
    let inner = frame.show(ui, |ui| {
        ui.set_width((width - 26.0).max(80.0));
        ui.set_min_height(CONTROL_H - 2.0);
        ui.horizontal_centered(|ui| {
            ui.add(Icon::Search.image(icons::SIZE, t.text_dim));
            let clear = if text.is_empty() { 0.0 } else { 26.0 };
            let edit = egui::TextEdit::singleline(text)
                .hint_text(hint)
                .frame(egui::Frame::NONE)
                .desired_width(ui.available_width() - clear)
                .vertical_align(egui::Align::Center);
            let response = ui.add(edit);
            if focus {
                response.request_focus();
            }
            if !text.is_empty() && icons::button_small(ui, t, Icon::X, "Clear search").clicked() {
                text.clear();
            }
            response
        })
        .inner
    });
    inner.inner
}

/// A centred title, one line of guidance and optionally one primary action. True when clicked.
pub fn empty_state(ui: &mut Ui, t: &Tokens, title: &str, hint: &str, action: Option<&str>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.35).max(24.0));
        ui.heading(title);
        ui.label(egui::RichText::new(hint).color(t.text_dim));
        if let Some(label) = action {
            ui.add_space(8.0);
            clicked = primary_button(ui, t, label, true).clicked();
        }
    });
    clicked
}

/// The one filled button of a group.
pub fn primary_button(ui: &mut Ui, t: &Tokens, label: &str, enabled: bool) -> Response {
    let button = egui::Button::new(egui::RichText::new(label).color(t.on_accent)).fill(t.accent);
    ui.add_enabled(enabled, button)
}

/// The filled button with a leading icon, as wide as the available space. Disabled, it keeps the
/// accent hue but muted, instead of egui's grey fade, and explains itself in `why_disabled`.
pub fn primary_wide(ui: &mut Ui, t: &Tokens, icon: Icon, label: &str, why_disabled: Option<&str>) -> Response {
    let (fill, text) = if why_disabled.is_some() {
        (t.accent.gamma_multiply(0.45), t.on_accent.gamma_multiply(0.75))
    } else {
        (t.accent, t.on_accent)
    };
    let button = egui::Button::image_and_text(icon.image(icons::SIZE, text), egui::RichText::new(label).color(text).strong())
        .fill(fill)
        .min_size(vec2(ui.available_width(), CONTROL_H));
    match why_disabled {
        Some(why) => ui.add(button.sense(Sense::hover())).on_hover_text(why),
        None => ui.add(button),
    }
}
