//! Painted building blocks of the main window: folder rows, message rows, empty states.
//! All colours come from [`Tokens`]; all sizes are named here or in `theme`.

use beavermail_core::MessageSummary;
use eframe::egui::{self, CornerRadius, FontId, Rect, Response, Sense, Ui, pos2, vec2};
use egui::text::{LayoutJob, TextWrapping};

use crate::theme::{CONTROL_RADIUS, Tokens};

/// Fixed row height, so the list can be virtualised with `show_rows`.
pub const MESSAGE_ROW_H: f32 = 72.0;
const FOLDER_ROW_H: f32 = 36.0;

fn tint(t: &Tokens) -> egui::Color32 {
    t.accent.gamma_multiply(0.16)
}

/// Single-line text that ends in an ellipsis instead of overflowing `width`.
fn truncated(ui: &Ui, text: &str, font: FontId, color: egui::Color32, width: f32) -> std::sync::Arc<egui::Galley> {
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

/// A folder in the sidebar: name on the left, unread count on the right.
pub fn folder_row(ui: &mut Ui, t: &Tokens, name: &str, unread: u32, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), FOLDER_ROW_H), Sense::click());
    row_background(ui, rect, &response, selected, t);
    let color = if selected { t.accent } else { t.text };
    let font = FontId::proportional(15.0);
    let right = if unread > 0 {
        let galley = ui.painter().layout_no_wrap(unread.to_string(), FontId::proportional(13.0), if selected { t.accent } else { t.text_dim });
        let width = galley.size().x;
        ui.painter().galley(pos2(rect.right() - 12.0 - width, rect.center().y - galley.size().y / 2.0), galley, t.text_dim);
        width + 8.0
    } else {
        0.0
    };
    let galley = truncated(ui, name, font, color, rect.width() - 24.0 - right);
    ui.painter().galley(pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0), galley, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A message in the list: unread dot, sender, date, subject and a one-line preview.
pub fn message_row(ui: &mut Ui, t: &Tokens, message: &MessageSummary, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), MESSAGE_ROW_H), Sense::click());
    let card = rect.shrink2(vec2(8.0, 2.0));
    row_background(ui, card, &response, selected, t);
    let left = card.left() + 28.0;
    let width = card.right() - 14.0 - left;

    if message.unread {
        ui.painter().circle_filled(pos2(card.left() + 14.0, card.top() + 20.0), 4.0, t.accent);
    }
    let date = ui.painter().layout_no_wrap(message.received.clone(), FontId::proportional(13.0), t.text_dim);
    let date_w = date.size().x;
    ui.painter().galley(pos2(card.right() - 14.0 - date_w, card.top() + 12.0), date, t.text_dim);

    let primary = if message.unread { t.text } else { t.text_dim };
    let sender = truncated(ui, &message.from, FontId::proportional(15.0), primary, width - date_w - 12.0);
    ui.painter().galley(pos2(left, card.top() + 10.0), sender, primary);
    let subject = truncated(ui, &message.subject, FontId::proportional(14.0), primary, width);
    ui.painter().galley(pos2(left, card.top() + 29.0), subject, primary);
    let preview = truncated(ui, &message.preview, FontId::proportional(13.0), t.text_dim, width);
    ui.painter().galley(pos2(left, card.top() + 48.0), preview, t.text_dim);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
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
