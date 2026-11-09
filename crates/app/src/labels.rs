// SPDX-License-Identifier: AGPL-3.0-only

//! Settings › Labels: the labels' names and colours, kept on this computer (the server keeps only the
//! keyword on each message), and the user's own labels made and removed.

use beavermail_core::Label;
use eframe::egui::{self, RichText, vec2};

use crate::theme::{self, STANDARD_LABELS, Tokens};

/// The page. True when the labels changed and are to be saved.
pub(crate) fn page(ui: &mut egui::Ui, t: &Tokens, labels: &mut Vec<Label>) -> bool {
    let mut changed = false;
    ui.label(RichText::new("Names and colours are kept on this computer. Each message carries only a label's keyword, which other clients that use the same keywords (Thunderbird) show too.").size(13.0).color(t.text_dim));
    ui.add_space(12.0);
    let mut remove = None;
    for (keyword, name, color) in theme::labels() {
        let standard = STANDARD_LABELS.iter().find(|(k, ..)| *k == keyword);
        ui.horizontal(|ui| {
            // The colour, picked from the calendars' palette.
            let (rect, dot) = ui.allocate_exact_size(vec2(theme::CONTROL_H, theme::CONTROL_H), egui::Sense::click());
            ui.painter().circle_filled(rect.center(), 9.0, color);
            if dot.hovered() {
                ui.painter().circle_stroke(rect.center(), 12.0, egui::Stroke::new(1.0, t.border_strong));
            }
            let dot = dot.on_hover_text("Change its colour").on_hover_cursor(egui::CursorIcon::PointingHand);
            // Stays open while a colour is dragged in the picker; a click outside closes it.
            egui::Popup::from_toggle_button_response(&dot).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.horizontal(|ui| {
                    for hex in theme::CALENDAR_COLORS {
                        let swatch = theme::calendar_color(hex).unwrap_or(color);
                        let (rect, pick) = ui.allocate_exact_size(vec2(28.0, 28.0), egui::Sense::click());
                        ui.painter().circle_filled(rect.center(), 10.0, swatch);
                        if swatch == color {
                            ui.painter().circle_stroke(rect.center(), 13.0, egui::Stroke::new(2.0, t.accent));
                        }
                        if pick.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            entry(labels, &keyword).color = hex.to_owned();
                            changed = true;
                        }
                    }
                });
                ui.add_space(12.0);
                // Any other colour.
                if let Some(picked) = picker(ui, t, egui::Id::new(("label_color", &keyword)), color) {
                    entry(labels, &keyword).color = hex(picked);
                    changed = true;
                }
            });
            let mut text = name.clone();
            let field = ui.add(crate::sized_field(&mut text, 260.0).hint_text("Name"));
            if field.changed() && !text.trim().is_empty() {
                entry(labels, &keyword).name = text;
                changed = true;
            }
            match standard {
                // A standard one comes back as it was; the user's own goes.
                Some((_, default, _)) => {
                    let kept = labels.iter().any(|l| l.keyword == keyword);
                    if ui.add_enabled(kept, egui::Button::new("Reset")).on_hover_text(format!("Back to {default} in its usual colour")).on_disabled_hover_text("Already as it comes").clicked() {
                        labels.retain(|l| l.keyword != keyword);
                        changed = true;
                    }
                }
                None => {
                    if crate::icons::button(ui, t, crate::icons::Icon::Trash, "Delete label: messages keep its keyword on the server", true, false).clicked() {
                        remove = Some(keyword.clone());
                    }
                }
            }
        });
    }
    if let Some(keyword) = remove {
        labels.retain(|l| l.keyword != keyword);
        changed = true;
    }
    ui.add_space(8.0);
    if ui.button("New label").clicked() {
        let name = (1..).map(|n| if n == 1 { "New label".to_owned() } else { format!("New label {n}") }).find(|n| !theme::labels().iter().any(|(_, have, _)| have == n)).unwrap_or_default();
        let taken: Vec<String> = theme::labels().into_iter().map(|(k, ..)| k).collect();
        labels.push(Label { keyword: keyword_for(&name, &taken), name, color: theme::CALENDAR_COLORS[labels.len() % theme::CALENDAR_COLORS.len()].to_owned() });
        changed = true;
    }
    changed
}

/// The label kept here for `keyword`, made with its current name and colour if there is none yet.
fn entry<'a>(labels: &'a mut Vec<Label>, keyword: &str) -> &'a mut Label {
    if let Some(at) = labels.iter().position(|l| l.keyword == keyword) {
        return &mut labels[at];
    }
    let (name, color) = theme::label(keyword);
    labels.push(Label { keyword: keyword.to_owned(), name, color: hex(color) });
    labels.last_mut().expect("just pushed")
}

/// The width of the colour popup's picker: as wide as the row of quick colours above it.
const PICKER_W: f32 = 284.0;

/// A colour picker the width of its popup: saturation and brightness in an area, the hue in a bar
/// under it, then the colour as `#RRGGBB` to read or paste. `Some` with the colour picked.
fn picker(ui: &mut egui::Ui, t: &Tokens, id: egui::Id, color: egui::Color32) -> Option<egui::Color32> {
    use egui::ecolor::HsvaGamma as Hsva;
    // The hue is kept apart: a grey or black has none of its own, and dragging there must not lose it.
    let mut hsva = ui.data(|d| d.get_temp::<(egui::Color32, Hsva)>(id)).filter(|(c, _)| *c == color).map_or_else(|| Hsva::from(color), |(_, h)| h);
    let before = hsva;
    let at = |h: f32, s: f32, v: f32| egui::Color32::from(Hsva { h, s, v, a: 1.0 });

    // Saturation across, brightness down, in the hue chosen.
    let (area, response) = ui.allocate_exact_size(vec2(PICKER_W, 150.0), egui::Sense::click_and_drag());
    const STEPS: u32 = 16;
    let mut mesh = egui::Mesh::default();
    for row in 0..=STEPS {
        for col in 0..=STEPS {
            let (s, v) = (col as f32 / STEPS as f32, 1.0 - row as f32 / STEPS as f32);
            mesh.colored_vertex(egui::pos2(area.left() + s * area.width(), area.top() + (1.0 - v) * area.height()), at(hsva.h, s, v));
        }
    }
    for row in 0..STEPS {
        for col in 0..STEPS {
            let i = row * (STEPS + 1) + col;
            mesh.add_triangle(i, i + 1, i + STEPS + 2);
            mesh.add_triangle(i, i + STEPS + 2, i + STEPS + 1);
        }
    }
    ui.painter().add(mesh);
    ui.painter().rect_stroke(area, egui::CornerRadius::same(4), egui::Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
    if let Some(p) = response.interact_pointer_pos().filter(|_| response.dragged() || response.clicked()) {
        hsva.s = ((p.x - area.left()) / area.width()).clamp(0.0, 1.0);
        hsva.v = 1.0 - ((p.y - area.top()) / area.height()).clamp(0.0, 1.0);
    }
    let mark = egui::pos2(area.left() + hsva.s * area.width(), area.top() + (1.0 - hsva.v) * area.height());
    ui.painter().circle_stroke(mark, 7.0, egui::Stroke::new(3.0, egui::Color32::from_black_alpha(90)));
    ui.painter().circle_stroke(mark, 7.0, egui::Stroke::new(2.0, egui::Color32::WHITE));
    response.on_hover_cursor(egui::CursorIcon::Crosshair);

    // The hue, round the colour wheel.
    ui.add_space(10.0);
    let (bar, response) = ui.allocate_exact_size(vec2(PICKER_W, 14.0), egui::Sense::click_and_drag());
    let mut mesh = egui::Mesh::default();
    const HUES: u32 = 36;
    for i in 0..=HUES {
        let x = bar.left() + i as f32 / HUES as f32 * bar.width();
        mesh.colored_vertex(egui::pos2(x, bar.top()), at(i as f32 / HUES as f32, 1.0, 1.0));
        mesh.colored_vertex(egui::pos2(x, bar.bottom()), at(i as f32 / HUES as f32, 1.0, 1.0));
    }
    for i in 0..HUES {
        mesh.add_triangle(2 * i, 2 * i + 1, 2 * i + 3);
        mesh.add_triangle(2 * i, 2 * i + 3, 2 * i + 2);
    }
    ui.painter().add(mesh);
    if let Some(p) = response.interact_pointer_pos().filter(|_| response.dragged() || response.clicked()) {
        hsva.h = ((p.x - bar.left()) / bar.width()).clamp(0.0, 0.999);
    }
    let x = bar.left() + hsva.h * bar.width();
    let knob = egui::Rect::from_center_size(egui::pos2(x, bar.center().y), vec2(8.0, 20.0));
    ui.painter().rect(knob, egui::CornerRadius::same(4), at(hsva.h, 1.0, 1.0), egui::Stroke::new(2.0, egui::Color32::WHITE), egui::StrokeKind::Inside);
    response.on_hover_cursor(egui::CursorIcon::PointingHand);

    // The colour as text, kept while it is being typed.
    ui.add_space(12.0);
    let mut picked = (hsva != before).then(|| egui::Color32::from(hsva));
    ui.horizontal(|ui| {
        let (swatch, _) = ui.allocate_exact_size(vec2(theme::CONTROL_H, theme::CONTROL_H), egui::Sense::hover());
        ui.painter().rect(swatch, egui::CornerRadius::same(theme::CONTROL_RADIUS), picked.unwrap_or(color), egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        let text_id = id.with("hex");
        let mut text = ui.data(|d| d.get_temp::<String>(text_id)).unwrap_or_else(|| hex(color));
        let field = ui.add(crate::sized_field(&mut text, 140.0).hint_text("#3B82F6").font(egui::TextStyle::Monospace));
        if field.changed()
            && let Some(typed) = theme::calendar_color(&format!("#{}", text.trim().trim_start_matches('#')))
        {
            picked = Some(typed);
            hsva = Hsva::from(typed);
        }
        // Shows the colour picked elsewhere unless the user is typing in it.
        if !field.has_focus() {
            text = hex(picked.unwrap_or(color));
        }
        ui.data_mut(|d| d.insert_temp(text_id, text));
    });
    let shown = picked.unwrap_or(color);
    ui.data_mut(|d| d.insert_temp(id, (shown, hsva)));
    picked.filter(|p| *p != color)
}

/// A colour as kept in the settings: `#RRGGBB`.
fn hex(color: egui::Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

/// The keyword for a new label named `name`, as Thunderbird makes one ("To read" is `to_read`):
/// letters, digits and `_` only, so every server takes it, and not one of `taken`.
fn keyword_for(name: &str, taken: &[String]) -> String {
    let mut base = String::new();
    for c in name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }) {
        if !(c == '_' && base.ends_with('_')) {
            base.push(c);
        }
    }
    let base = base.trim_matches('_');
    let base = if base.is_empty() { "label" } else { base };
    (1..).map(|n| if n == 1 { base.to_owned() } else { format!("{base}_{n}") }).find(|k| !taken.iter().any(|t| t.eq_ignore_ascii_case(k))).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_labels_keyword_is_a_plain_word_of_its_own() {
        assert_eq!(keyword_for("To read", &[]), "to_read");
        assert_eq!(keyword_for("Città / 2026", &[]), "citt_2026");
        assert_eq!(keyword_for("  ", &[]), "label");
        assert_eq!(keyword_for("Work", &["work".into(), "work_2".into()]), "work_3");
    }
}
