// SPDX-License-Identifier: AGPL-3.0-only

//! The left column of the Calendar section: a month to pick a day from, and the calendars, each a
//! source of its own (synced over CalDAV, or on this computer), shown or hidden.

use std::collections::{BTreeSet, HashSet};

use beavermail_core::CalendarSource;
use chrono::{Datelike, Duration, NaiveDate};
use eframe::egui::{self, Color32, CornerRadius, FontId, Rect, RichText, Sense, pos2, vec2};

use crate::books::{self, Words};
use crate::theme::{self, Tokens};

pub const CALENDARS: Words = Words {
    id: "add_calendar",
    one: "calendar",
    many: "calendars",
    about: "See your events from a CalDAV server. Often the same login as your mail.",
    read_only: "Read-only: shared with you, or a subscription",
};

pub enum CalendarAction {
    /// Open the dialog to add calendars.
    Add,
    /// Sync these calendars from now on, with the dialog's login.
    Added(Vec<beavermail_carddav::Found>),
    /// Show or hide this calendar's events (by URL).
    Toggle(String),
    /// Accepted invitations go to this one.
    MakeDefault(String),
    SyncNow,
    /// Ask before no longer syncing this one.
    AskRemove(String),
    /// Fold or unfold a server's calendars.
    Fold(String),
    /// A day picked in the month.
    Pick(NaiveDate),
    /// The month shown, a month earlier (-1) or later (1).
    Month(i32),
}

/// The server a calendar is on, as the column and Settings name it: "This computer", or login@host.
pub fn server(calendar: &CalendarSource) -> String {
    if calendar.is_local() { "This computer".to_owned() } else { format!("{}@{}", calendar.user, books::host(&calendar.url)) }
}

/// A server's password being changed in Settings: what is typed, and whether it is being checked or
/// why the server refused it.
#[derive(Default)]
pub struct Login {
    pub password: String,
    pub checking: bool,
    pub error: Option<String>,
}

/// What the Calendars page of Settings asks for; names, colours and the default change in place.
pub enum PageAction {
    /// Check the password typed for this server (by [`server`]) and keep it.
    Password(String),
    /// Ask before no longer syncing these calendars (by URL).
    AskRemove(Vec<String>),
    /// Open the dialog to add calendars.
    Add,
}

/// The Calendars page of Settings: each server with its calendars (colour, name, default, remove),
/// its password and Remove server. Returns whether a calendar changed, and what else to do.
pub fn page(ui: &mut egui::Ui, t: &Tokens, calendars: &mut [CalendarSource], logins: &mut std::collections::HashMap<String, Login>) -> (bool, Option<PageAction>) {
    let (mut changed, mut action) = (false, None);
    let mut servers: Vec<String> = Vec::new();
    for c in calendars.iter().filter(|c| c.is_local()).chain(calendars.iter().filter(|c| !c.is_local())) {
        if !servers.contains(&server(c)) {
            servers.push(server(c));
        }
    }
    let mut make_default = None;
    for name in servers {
        ui.add_space(20.0);
        ui.label(RichText::new(&name).size(theme::SUBTITLE).family(theme::bold()));
        ui.add_space(4.0);
        let urls: Vec<String> = calendars.iter().filter(|c| server(c) == name).map(|c| c.url.clone()).collect();
        for (at, calendar) in calendars.iter_mut().enumerate().filter(|(_, c)| server(c) == name) {
            ui.horizontal(|ui| {
                let (swatch, _) = ui.allocate_exact_size(vec2(theme::CONTROL_H, theme::CONTROL_H), Sense::hover());
                let pick = ui.interact(swatch, ui.id().with(("swatch", &calendar.url)), Sense::click()).on_hover_text("Colour").on_hover_cursor(egui::CursorIcon::PointingHand);
                ui.painter().rect_filled(Rect::from_center_size(swatch.center(), vec2(18.0, 18.0)), CornerRadius::same(4), color(calendar, at));
                egui::Popup::menu(&pick).style(crate::views::menu_look).show(|ui| {
                    ui.horizontal(|ui| {
                        for hex in theme::CALENDAR_COLORS {
                            let fill = theme::calendar_color(hex).unwrap_or(t.accent);
                            let (r, chip) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                            ui.painter().rect_filled(r.shrink(3.0), CornerRadius::same(4), fill);
                            if calendar.color.eq_ignore_ascii_case(hex) {
                                ui.painter().rect_stroke(r, CornerRadius::same(6), egui::Stroke::new(2.0, t.text), egui::StrokeKind::Inside);
                            }
                            if chip.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                calendar.color = hex.to_owned();
                                changed = true;
                                ui.close();
                            }
                        }
                    });
                });
                let width = (ui.available_width() - 230.0).max(160.0);
                changed |= ui.add(crate::sized_field(&mut calendar.name, width).id(ui.id().with(("calendar_name", &calendar.url)))).changed();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !calendar.is_local() && ui.button("Remove").on_hover_text("Stop syncing it: its events leave BeaverMail, nothing changes on the server").clicked() {
                        action = Some(PageAction::AskRemove(vec![calendar.url.clone()]));
                    }
                    if calendar.default {
                        ui.label(RichText::new("Default").color(t.text_dim)).on_hover_text("Accepted invitations and new events go here");
                    } else if calendar.read_only {
                        ui.label(RichText::new("Read-only").color(t.text_dim)).on_hover_text("Shared with you or a subscription: nothing can be written there");
                    } else if ui.button("Make default").on_hover_text("Accepted invitations and new events go here").clicked() {
                        make_default = Some(calendar.url.clone());
                    }
                });
            });
        }
        if name == "This computer" {
            continue;
        }
        ui.add_space(8.0);
        let login = logins.entry(name.clone()).or_default();
        ui.horizontal(|ui| {
            ui.label("Password");
            let field = ui.add_enabled(!login.checking, crate::sized_field(&mut login.password, 220.0).password(true).hint_text("New password"));
            if field.changed() {
                login.error = None;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let can = !login.password.is_empty() && !login.checking;
            if ui.add_enabled(can, egui::Button::new("Save password")).on_hover_text("Checked on the server first").on_disabled_hover_text("Type the new password first").clicked() || (can && enter) {
                action = Some(PageAction::Password(name.clone()));
            }
            if login.checking {
                ui.spinner();
            }
        });
        if let Some(error) = &login.error {
            ui.add(egui::Label::new(RichText::new(error).size(13.0).color(t.danger)).wrap());
        }
        ui.add_space(4.0);
        let count = urls.len();
        let label = if count == 1 { "Remove server…".to_owned() } else { format!("Remove server and its {count} calendars…") };
        if ui.button(RichText::new(label).color(t.danger)).clicked() {
            action = Some(PageAction::AskRemove(urls));
        }
    }
    if let Some(url) = make_default {
        for c in calendars.iter_mut() {
            c.default = c.url == url;
        }
        changed = true;
    }
    ui.add_space(20.0);
    if ui.button("Add calendar…").clicked() {
        action = Some(PageAction::Add);
    }
    (changed, action)
}

/// The colour of a calendar: its own, or one of the app's by its place in the list.
pub fn color(calendar: &CalendarSource, at: usize) -> Color32 {
    theme::calendar_color(&calendar.color).unwrap_or_else(|| theme::account_color(at))
}

/// The Monday on or before `day`.
pub fn week_start(day: NaiveDate) -> NaiveDate {
    day - Duration::days(i64::from(day.weekday().num_days_from_monday()))
}

/// A month as a grid to pick a day from (weeks from Monday), the days with events marked; the day
/// picked and today stand out.
pub fn mini_month(ui: &mut egui::Ui, t: &Tokens, month: NaiveDate, picked: NaiveDate, today: NaiveDate, busy: &HashSet<NaiveDate>) -> Option<CalendarAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(month.format("%B %Y").to_string()).size(theme::SUBTITLE).family(theme::bold()).color(t.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::icons::button(ui, t, crate::icons::Icon::ChevronRight, "Next month", true, false).clicked() {
                action = Some(CalendarAction::Month(1));
            }
            if crate::icons::button(ui, t, crate::icons::Icon::ChevronLeft, "Previous month", true, false).clicked() {
                action = Some(CalendarAction::Month(-1));
            }
        });
    });
    ui.add_space(4.0);
    let cell = (ui.available_width() / 7.0).floor();
    let (rect, _) = ui.allocate_exact_size(vec2(cell * 7.0, 24.0), Sense::hover());
    for (i, name) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
        ui.painter().text(pos2(rect.left() + cell * (i as f32 + 0.5), rect.center().y), egui::Align2::CENTER_CENTER, name, FontId::proportional(12.0), t.text_dim);
    }
    let first = week_start(month);
    for week in 0..6 {
        let (row, _) = ui.allocate_exact_size(vec2(cell * 7.0, 32.0), Sense::hover());
        for d in 0..7 {
            let day = first + Duration::days(week * 7 + d);
            let r = Rect::from_min_size(pos2(row.left() + cell * d as f32, row.top()), vec2(cell, 32.0));
            let response = ui.interact(r, ui.id().with(("mini", day)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            let circle = r.center() - vec2(0.0, 2.0);
            let color = if day == picked {
                ui.painter().circle_filled(circle, 13.0, t.accent);
                t.on_accent
            } else {
                if response.hovered() {
                    ui.painter().circle_filled(circle, 13.0, t.hover);
                }
                if day == today {
                    ui.painter().circle_stroke(circle, 13.0, egui::Stroke::new(1.5, t.accent));
                }
                if day.month() == month.month() { t.text } else { t.text_dim }
            };
            ui.painter().text(circle, egui::Align2::CENTER_CENTER, day.day().to_string(), FontId::proportional(13.0), color);
            if busy.contains(&day) {
                ui.painter().circle_filled(pos2(circle.x, r.bottom() - 3.0), 2.0, if day == picked { t.accent } else { t.text_dim });
            }
            if response.clicked() {
                action = Some(CalendarAction::Pick(day));
            }
        }
    }
    action
}

/// The calendars, under the server they are on (the one on this computer first), as the Mail column
/// lists folders under each account: a click shows or hides one; right-click to make it the default,
/// sync it, or stop syncing it. `folded`: the servers folded.
pub fn section(ui: &mut egui::Ui, t: &Tokens, calendars: &[CalendarSource], syncing: &HashSet<String>, folded: &BTreeSet<String>) -> Option<CalendarAction> {
    let mut action = None;
    ui.label(RichText::new("CALENDARS").size(12.0).color(t.text_dim));
    let mut servers: Vec<String> = Vec::new();
    for c in calendars.iter().filter(|c| c.is_local()).chain(calendars.iter().filter(|c| !c.is_local())) {
        if !servers.contains(&server(c)) {
            servers.push(server(c));
        }
    }
    for name in servers {
        let key = format!("caldav:{name}");
        let header = egui::CollapsingHeader::new(RichText::new(&name).size(15.0).color(t.text)).id_salt(&key).default_open(!folded.contains(&key)).icon(crate::views::fold_arrow).show(ui, |ui| {
            for (at, calendar) in calendars.iter().enumerate().filter(|(_, c)| server(c) == name) {
                let label = match (calendar.default, syncing.contains(&calendar.url)) {
                    (_, true) => format!("{}  ·  syncing", calendar.name),
                    (true, false) => format!("{}  ·  default", calendar.name),
                    (false, false) => calendar.name.clone(),
                };
                let row = calendar_row(ui, t, &label, color(calendar, at), calendar.shown);
                let tip = if calendar.shown { "Shown: click to hide its events" } else { "Hidden: click to show its events" };
                let row = row.on_hover_text(tip);
                if row.clicked() {
                    action = Some(CalendarAction::Toggle(calendar.url.clone()));
                }
                crate::views::context_menu(&row, |ui| {
                    let why = if calendar.read_only { "Read-only: answers to invitations cannot go there" } else { "Accepted invitations already go here" };
                    if ui.add_enabled(!calendar.default && !calendar.read_only, egui::Button::new("Make default")).on_disabled_hover_text(why).clicked() {
                        action = Some(CalendarAction::MakeDefault(calendar.url.clone()));
                        ui.close();
                    }
                    if !calendar.is_local() {
                        if ui.button("Sync now").clicked() {
                            action = Some(CalendarAction::SyncNow);
                            ui.close();
                        }
                        if ui.button(RichText::new("Stop syncing…").color(t.danger)).clicked() {
                            action = Some(CalendarAction::AskRemove(calendar.url.clone()));
                            ui.close();
                        }
                    }
                });
            }
        });
        if header.header_response.clicked() {
            action = Some(CalendarAction::Fold(key));
        }
    }
    action
}

/// A calendar in the list: a square of its colour, filled while it is shown, and its name.
fn calendar_row(ui: &mut egui::Ui, t: &Tokens, name: &str, color: Color32, shown: bool) -> egui::Response {
    let height = 36.0_f32.min(ui.spacing().interact_size.y); // see theme::dense
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::CONTROL_RADIUS), t.hover);
    }
    let square = Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(16.0, 16.0));
    if shown {
        ui.painter().rect_filled(square, CornerRadius::same(4), color);
    } else {
        ui.painter().rect_stroke(square, CornerRadius::same(4), egui::Stroke::new(1.5, color), egui::StrokeKind::Inside);
    }
    let text = crate::views::truncated(ui, name, FontId::proportional(15.0), if shown { t.text } else { t.text_dim }, rect.width() - 44.0);
    ui.painter().galley(pos2(rect.left() + 36.0, rect.center().y - text.size().y / 2.0), text, t.text);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, shown, name));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_page_lists_each_server_with_its_calendars_within_the_page() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let calendar = |name: &str, url: &str, user: &str| CalendarSource { name: name.into(), url: url.into(), user: user.into(), color: String::new(), shown: true, default: false, read_only: false };
        let mut calendars = vec![
            calendar("On this computer", CalendarSource::LOCAL, ""),
            calendar("Work", "https://dav.example.org/ada/work/", "ada"),
            calendar("A calendar with a rather long name for the field", "https://dav.example.org/ada/home/", "ada"),
            calendar("Family", "https://cal.example.com/bob/family/", "bob"),
        ];
        calendars[1].default = true;
        assert_eq!(server(&calendars[0]), "This computer");
        assert_eq!(server(&calendars[3]), "bob@cal.example.com");
        let mut logins = std::collections::HashMap::new();
        let mut result = (true, Some(PageAction::Add));
        let mut used = egui::Rect::NOTHING;
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 1600.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            ui.allocate_ui(vec2(480.0, 1500.0), |ui| {
                result = page(ui, &Tokens::dark(), &mut calendars, &mut logins);
                used = ui.min_rect();
            });
        });
        output.textures_delta.clear(); // nothing is drawn here
        assert!(!result.0 && result.1.is_none(), "nothing changes by itself");
        assert!(used.width() <= 481.0, "within the page: {used:?}");
        assert_eq!(logins.len(), 2, "a password row for each server, none for this computer");
    }

    #[test]
    fn a_week_starts_on_monday_and_colours_come_from_the_server_or_the_app() {
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        assert_eq!(week_start(day(21)), day(19));
        assert_eq!(week_start(day(19)), day(19));
        assert_eq!(week_start(day(25)), day(19), "Sunday ends the week");
        let mut work = CalendarSource { name: "Work".into(), url: "https://dav.example.org/work/".into(), user: "ada".into(), color: "#3366FF".into(), shown: true, default: false, read_only: false };
        assert_eq!(color(&work, 0), Color32::from_rgb(0x33, 0x66, 0xFF));
        work.color.clear();
        assert_eq!(color(&work, 2), theme::account_color(2));
    }
}
