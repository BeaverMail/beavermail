// SPDX-License-Identifier: AGPL-3.0-only

//! The views of the Calendar section: a week of hours, a month, the coming days as a list, and an
//! event's details. Times are this computer's; an all-day event keeps its dates wherever it is shown.

use beavermail_carddav::calendar::Occurrence;
use beavermail_carddav::ical;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Offset, TimeZone, Timelike, Utc};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::icons::{self, Icon};
use crate::theme::{self, Tokens};
use crate::views;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Week,
    Month,
    Agenda,
}

/// An occurrence as a view shows it: which calendar it is in, and that calendar's colour.
#[derive(Clone, Debug)]
pub struct Item {
    pub occurrence: Occurrence,
    pub calendar: String,
    pub color: Color32,
}

impl Item {
    /// The days it is on, here: an all-day one from its dates (its end day excluded), a timed one from
    /// this computer's clock.
    fn days(&self) -> (NaiveDate, NaiveDate) {
        let o = &self.occurrence;
        if o.all_day {
            let last = (o.end - Duration::seconds(1)).date_naive().max(o.start.date_naive());
            (o.start.date_naive(), last)
        } else {
            let end = if o.end > o.start { o.end - Duration::seconds(1) } else { o.end };
            (o.start.with_timezone(&Local).date_naive(), end.with_timezone(&Local).date_naive())
        }
    }

    pub fn on(&self, day: NaiveDate) -> bool {
        let (first, last) = self.days();
        first <= day && day <= last
    }

    /// "09:30 – 10:30", or "All day".
    fn hours(&self) -> String {
        let o = &self.occurrence;
        if o.all_day {
            return "All day".into();
        }
        let (start, end) = (o.start.with_timezone(&Local), o.end.with_timezone(&Local));
        if end <= start { start.format("%H:%M").to_string() } else { format!("{} – {}", start.format("%H:%M"), end.format("%H:%M")) }
    }

    fn title(&self) -> &str {
        let summary = self.occurrence.event.summary.trim();
        if summary.is_empty() { "(no title)" } else { summary }
    }
}

/// What the user asked of a view.
pub enum ViewAction {
    Open(usize),
    /// A day picked in the month: its week is shown.
    Day(NaiveDate),
    /// An empty time clicked in the week: a new event then, minutes past midnight.
    New(NaiveDate, u32),
}

/// What the user asked of an event's details.
pub enum EventAction {
    Edit,
    Delete,
}

/// The range a view shows around `day`, as (first day, days).
pub fn range(view: View, day: NaiveDate) -> (NaiveDate, i64) {
    match view {
        View::Week => (crate::calendars::week_start(day), 7),
        View::Month => (crate::calendars::week_start(day.with_day(1).unwrap_or(day)), 42),
        View::Agenda => (day, 30),
    }
}

/// The view's title: "Oct 19 – 25, 2026" and its week number, "October 2026", or "From Mon 19 Oct".
pub fn title(view: View, day: NaiveDate) -> (String, String) {
    match view {
        View::Week => {
            let (from, _) = range(view, day);
            let to = from + Duration::days(6);
            let text = if from.month() == to.month() {
                format!("{} – {}, {}", from.format("%b %-d"), to.format("%-d"), to.year())
            } else {
                format!("{} – {}, {}", from.format("%b %-d"), to.format("%b %-d"), to.year())
            };
            (text, format!("W{}", from.iso_week().week()))
        }
        View::Month => (day.format("%B %Y").to_string(), String::new()),
        View::Agenda => (format!("From {}", day.format("%a %-d %b")), String::new()),
    }
}

/// Midnight of `day` here, in UTC.
pub fn midnight(day: NaiveDate) -> DateTime<Utc> {
    Local.from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap_or_default()).earliest().map_or_else(Utc::now, |t| t.with_timezone(&Utc))
}

/// How tall an hour is in the week view.
const HOUR: f32 = 48.0;
/// The hours' column on the left.
const GUTTER: f32 = 56.0;

/// A week of hours: the days across, an all-day row, then the day's hours with the events placed by
/// their times; side by side where they overlap; now as a line. `morning`: scroll to the working
/// morning (on a new week).
pub fn week(ui: &mut egui::Ui, t: &Tokens, from: NaiveDate, items: &[Item], today: NaiveDate, morning: bool) -> Option<ViewAction> {
    let mut action = None;
    let days: Vec<NaiveDate> = (0..7).map(|d| from + Duration::days(d)).collect();
    let column = ((ui.available_width() - GUTTER) / 7.0).floor();
    // The days.
    let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::hover());
    ui.painter().text(pos2(head.left() + GUTTER / 2.0, head.center().y), Align2::CENTER_CENTER, "LOCAL", FontId::proportional(11.0), t.text_dim);
    for (i, day) in days.iter().enumerate() {
        let x = head.left() + GUTTER + column * (i as f32 + 0.5);
        let color = if *day == today { t.accent } else { t.text_dim };
        ui.painter().text(pos2(x, head.top() + 14.0), Align2::CENTER_CENTER, day.format("%a").to_string().to_uppercase(), FontId::proportional(12.0), color);
        if *day == today {
            ui.painter().circle_filled(pos2(x, head.top() + 34.0), 13.0, t.accent);
        }
        ui.painter().text(pos2(x, head.top() + 34.0), Align2::CENTER_CENTER, day.day().to_string(), FontId::new(15.0, theme::bold()), if *day == today { t.on_accent } else { t.text });
    }
    // All-day events: up to three rows of chips, one per day they are on.
    let all_day: Vec<(usize, &Item)> = items.iter().enumerate().filter(|(_, i)| i.occurrence.all_day).collect();
    let rows = days.iter().map(|d| all_day.iter().filter(|(_, i)| i.on(*d)).count()).max().unwrap_or(0).min(3);
    let (band, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0 + 24.0 * rows.max(1) as f32), Sense::hover());
    ui.painter().text(pos2(band.left() + GUTTER / 2.0, band.top() + 16.0), Align2::CENTER_CENTER, "ALL DAY", FontId::proportional(11.0), t.text_dim);
    for (d, day) in days.iter().enumerate() {
        for (row, (at, item)) in all_day.iter().filter(|(_, i)| i.on(*day)).take(3).enumerate() {
            let r = Rect::from_min_size(pos2(band.left() + GUTTER + column * d as f32 + 2.0, band.top() + 4.0 + 24.0 * row as f32), vec2(column - 4.0, 21.0));
            if chip(ui, t, r, item, ("allday", *at, *day)).clicked() {
                action = Some(ViewAction::Open(*at));
            }
        }
    }
    ui.painter().hline(band.x_range(), band.bottom(), Stroke::new(1.0, t.border));
    // The hours.
    egui::ScrollArea::vertical().id_salt("week_hours").auto_shrink([false, false]).show(ui, |ui| {
        // Events are placed over it afterwards, so a click on one opens it and a click beside adds one.
        let (grid, empty) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0 * HOUR), Sense::click());
        // To the working morning, once the grid is laid out (an offset set before would be clamped to
        // nothing on the first frame, before the area knows how tall it is).
        if morning {
            ui.scroll_to_rect(Rect::from_min_size(pos2(grid.left(), grid.top() + 8.0 * HOUR - 8.0), vec2(1.0, 1.0)), Some(egui::Align::TOP));
        }
        let painter = ui.painter();
        for hour in 0..24 {
            let y = grid.top() + hour as f32 * HOUR;
            painter.hline(grid.left() + GUTTER..=grid.right(), y, Stroke::new(1.0, t.border));
            if hour > 0 {
                painter.text(pos2(grid.left() + GUTTER - 8.0, y), Align2::RIGHT_CENTER, format!("{hour:02}:00"), FontId::proportional(12.0), t.text_dim);
            }
        }
        for d in 0..=7 {
            let x = grid.left() + GUTTER + column * d as f32;
            painter.vline(x, grid.y_range(), Stroke::new(1.0, t.border));
        }
        for (d, day) in days.iter().enumerate() {
            let start = midnight(*day);
            let end = midnight(*day + Duration::days(1));
            let mut timed: Vec<(usize, &Item, f32, f32)> = items
                .iter()
                .enumerate()
                .filter(|(_, i)| !i.occurrence.all_day && i.occurrence.start < end && (i.occurrence.end > start || i.occurrence.start >= start))
                .map(|(at, i)| {
                    let minutes = |t: DateTime<Utc>| (t.clamp(start, end) - start).num_minutes() as f32;
                    (at, i, minutes(i.occurrence.start), minutes(i.occurrence.end).max(minutes(i.occurrence.start) + 20.0))
                })
                .collect();
            timed.sort_by(|a, b| a.2.total_cmp(&b.2));
            for (at, item, top, bottom, lane, lanes) in lanes(&timed) {
                let width = (column - 4.0) / lanes as f32;
                let r = Rect::from_min_max(
                    pos2(grid.left() + GUTTER + column * d as f32 + 2.0 + width * lane as f32, grid.top() + top * HOUR / 60.0 + 1.0),
                    pos2(grid.left() + GUTTER + column * d as f32 + 2.0 + width * (lane + 1) as f32 - 2.0, grid.top() + bottom * HOUR / 60.0 - 1.0),
                );
                if block(ui, t, r, item, ("timed", at, *day)).clicked() {
                    action = Some(ViewAction::Open(at));
                }
            }
        }
        if empty.clicked()
            && let Some(pos) = empty.interact_pointer_pos()
            && pos.x >= grid.left() + GUTTER
            && let Some(day) = days.get(((pos.x - grid.left() - GUTTER) / column) as usize)
        {
            // The half hour clicked.
            let minutes = ((pos.y - grid.top()) / HOUR * 2.0).floor().clamp(0.0, 47.0) as u32 * 30;
            action = Some(ViewAction::New(*day, minutes));
        }
        // Now, across today's column.
        if let Some(d) = days.iter().position(|d| *d == today) {
            let now = Local::now();
            let y = grid.top() + (now.hour() * 60 + now.minute()) as f32 * HOUR / 60.0;
            let x = grid.left() + GUTTER + column * d as f32;
            ui.painter().hline(x..=x + column, y, Stroke::new(2.0, t.danger));
            ui.painter().circle_filled(pos2(x, y), 4.0, t.danger);
        }
    });
    action
}

/// Each timed event of a day with its lane and how many lanes its group of overlapping events has.
fn lanes<'a>(sorted: &[(usize, &'a Item, f32, f32)]) -> Vec<(usize, &'a Item, f32, f32, usize, usize)> {
    let mut out = Vec::new();
    let mut group: Vec<(usize, &'a Item, f32, f32, usize)> = Vec::new();
    let mut ends: Vec<f32> = Vec::new();
    let mut group_end = f32::MIN;
    let mut flush = |group: &mut Vec<(usize, &'a Item, f32, f32, usize)>, ends: &mut Vec<f32>| {
        let count = ends.len().max(1);
        out.extend(group.drain(..).map(|(at, item, top, bottom, lane)| (at, item, top, bottom, lane, count)));
        ends.clear();
    };
    for &(at, item, top, bottom) in sorted {
        if top >= group_end {
            flush(&mut group, &mut ends);
            group_end = f32::MIN;
        }
        let lane = ends.iter().position(|e| *e <= top).unwrap_or(ends.len());
        if lane == ends.len() {
            ends.push(bottom);
        } else {
            ends[lane] = bottom;
        }
        group_end = group_end.max(bottom);
        group.push((at, item, top, bottom, lane));
    }
    flush(&mut group, &mut ends);
    out
}

/// An event placed in the hours: its colour as a tint and a bar, its title and time.
fn block(ui: &mut egui::Ui, t: &Tokens, r: Rect, item: &Item, id: impl std::hash::Hash + std::fmt::Debug) -> egui::Response {
    let response = ui.interact(r, ui.id().with(id), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\n{}", item.title(), item.hours()));
    let fill = if response.hovered() { item.color.gamma_multiply(0.38) } else { item.color.gamma_multiply(0.24) };
    ui.painter().rect_filled(r, CornerRadius::same(6), fill);
    ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(3.0, r.height())), CornerRadius::same(2), item.color);
    let painter = ui.painter().with_clip_rect(r.shrink(2.0));
    let title = views::truncated(ui, item.title(), FontId::new(13.0, theme::bold()), t.text, r.width() - 12.0);
    let title_h = title.size().y;
    painter.galley(pos2(r.left() + 8.0, r.top() + 4.0), title, t.text);
    if r.height() > title_h + 20.0 {
        painter.text(pos2(r.left() + 8.0, r.top() + 6.0 + title_h), Align2::LEFT_TOP, item.hours(), FontId::proportional(12.0), t.text_dim);
    }
    response
}

/// An all-day event, or one line of a month's day: a chip with its colour.
fn chip(ui: &mut egui::Ui, t: &Tokens, r: Rect, item: &Item, id: impl std::hash::Hash + std::fmt::Debug) -> egui::Response {
    let response = ui.interact(r, ui.id().with(id), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\n{}", item.title(), item.hours()));
    let fill = if response.hovered() { item.color.gamma_multiply(0.38) } else { item.color.gamma_multiply(0.24) };
    ui.painter().rect_filled(r, CornerRadius::same(5), fill);
    ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(3.0, r.height())), CornerRadius::same(2), item.color);
    let text = if item.occurrence.all_day { item.title().to_owned() } else { format!("{}  {}", item.occurrence.start.with_timezone(&Local).format("%H:%M"), item.title()) };
    let text = views::truncated(ui, &text, FontId::proportional(12.5), t.text, r.width() - 12.0);
    ui.painter().galley(pos2(r.left() + 8.0, r.center().y - text.size().y / 2.0), text, t.text);
    response
}

/// A month as six weeks from Monday: each day with up to three of its events and how many more.
pub fn month(ui: &mut egui::Ui, t: &Tokens, from: NaiveDate, month: u32, items: &[Item], today: NaiveDate) -> Option<ViewAction> {
    let mut action = None;
    let column = (ui.available_width() / 7.0).floor();
    let (head, _) = ui.allocate_exact_size(vec2(column * 7.0, 28.0), Sense::hover());
    for (i, name) in ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"].iter().enumerate() {
        ui.painter().text(pos2(head.left() + column * (i as f32 + 0.5), head.center().y), Align2::CENTER_CENTER, name, FontId::proportional(12.0), t.text_dim);
    }
    let row_h = ((ui.available_height() - 4.0) / 6.0).max(96.0);
    egui::ScrollArea::vertical().id_salt("month").auto_shrink([false, false]).show(ui, |ui| {
        let (grid, _) = ui.allocate_exact_size(vec2(column * 7.0, row_h * 6.0), Sense::hover());
        for cell in 0..42 {
            let day = from + Duration::days(cell);
            let r = Rect::from_min_size(pos2(grid.left() + column * (cell % 7) as f32, grid.top() + row_h * (cell / 7) as f32), vec2(column, row_h));
            ui.painter().rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
            let number = Rect::from_min_size(r.min + vec2(4.0, 4.0), vec2(26.0, 22.0));
            let picked = ui.interact(number, ui.id().with(("month_day", day)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Show this week");
            if day == today {
                ui.painter().circle_filled(number.center(), 11.0, t.accent);
            } else if picked.hovered() {
                ui.painter().circle_filled(number.center(), 11.0, t.hover);
            }
            let color = match (day == today, day.month() == month) {
                (true, _) => t.on_accent,
                (false, true) => t.text,
                (false, false) => t.text_dim,
            };
            ui.painter().text(number.center(), Align2::CENTER_CENTER, day.day().to_string(), FontId::proportional(13.0), color);
            if picked.clicked() {
                action = Some(ViewAction::Day(day));
            }
            let on_day: Vec<(usize, &Item)> = items.iter().enumerate().filter(|(_, i)| i.on(day)).collect();
            let fits = (((r.height() - 30.0) / 22.0).floor() as usize).max(1);
            let shown = if on_day.len() > fits { fits - 1 } else { on_day.len() };
            for (row, (at, item)) in on_day.iter().take(shown).enumerate() {
                let line = Rect::from_min_size(pos2(r.left() + 3.0, r.top() + 30.0 + 22.0 * row as f32), vec2(r.width() - 6.0, 20.0));
                if chip(ui, t, line, item, ("month", *at, day)).clicked() {
                    action = Some(ViewAction::Open(*at));
                }
            }
            if on_day.len() > shown {
                let more = Rect::from_min_size(pos2(r.left() + 6.0, r.top() + 30.0 + 22.0 * shown as f32), vec2(r.width() - 12.0, 20.0));
                let response = ui.interact(more, ui.id().with(("more", day)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Show this week");
                ui.painter().text(more.left_center(), Align2::LEFT_CENTER, format!("+{} more", on_day.len() - shown), FontId::proportional(12.5), t.text_dim);
                if response.clicked() {
                    action = Some(ViewAction::Day(day));
                }
            }
        }
    });
    action
}

/// The coming days with events, as a list: each day, then its events with their times and places.
pub fn agenda(ui: &mut egui::Ui, t: &Tokens, from: NaiveDate, days: i64, items: &[Item], today: NaiveDate) -> Option<ViewAction> {
    let mut action = None;
    egui::ScrollArea::vertical().id_salt("agenda").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(ui.available_width().min(760.0));
        let mut any = false;
        for d in 0..days {
            let day = from + Duration::days(d);
            let on_day: Vec<(usize, &Item)> = items.iter().enumerate().filter(|(_, i)| i.on(day)).collect();
            if on_day.is_empty() {
                continue;
            }
            any = true;
            let name = match (day - today).num_days() {
                0 => format!("Today  ·  {}", day.format("%a %-d %b")),
                1 => format!("Tomorrow  ·  {}", day.format("%a %-d %b")),
                _ => day.format("%A %-d %B").to_string(),
            };
            ui.add_space(14.0);
            ui.label(RichText::new(name).family(theme::bold()).color(if day == today { t.accent } else { t.text }));
            ui.add_space(4.0);
            for (at, item) in on_day {
                let (r, response) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(theme::CONTROL_RADIUS), t.hover);
                }
                ui.painter().text(pos2(r.left() + 10.0, r.center().y), Align2::LEFT_CENTER, item.hours(), FontId::proportional(13.0), t.text_dim);
                ui.painter().rect_filled(Rect::from_center_size(pos2(r.left() + 132.0, r.center().y), vec2(4.0, 32.0)), CornerRadius::same(2), item.color);
                let room = r.width() - 152.0;
                let title = views::truncated(ui, item.title(), FontId::new(14.5, theme::bold()), t.text, room);
                let place = &item.occurrence.event.location;
                let top = if place.is_empty() { r.center().y - title.size().y / 2.0 } else { r.top() + 6.0 };
                ui.painter().galley(pos2(r.left() + 144.0, top), title, t.text);
                if !place.is_empty() {
                    let place = views::truncated(ui, place, FontId::proportional(13.0), t.text_dim, room);
                    ui.painter().galley(pos2(r.left() + 144.0, r.top() + 26.0), place, t.text_dim);
                }
                if response.clicked() {
                    action = Some(ViewAction::Open(at));
                }
            }
        }
        if !any {
            views::empty_state(ui, t, "Nothing coming up", "No events in the next 30 days in the calendars shown.", None);
        }
    });
    action
}

/// An event's details: what, when (in its own zone too when that is another), where, which calendar,
/// who organises it and who is invited with their answers, and its description. Returns a link
/// clicked, cleaned of tracking; `close` is set when it is closed, `act` when Edit or Delete is
/// pressed. `can`: whether it can be changed and deleted here, or why not.
pub fn details(ui: &mut egui::Ui, t: &Tokens, item: &Item, calendar: &str, can: (Result<(), &str>, Result<(), &str>), close: &mut bool, act: &mut Option<EventAction>) -> Option<String> {
    let event = &item.occurrence.event;
    let mut url = None;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            *close = icons::button(ui, t, Icon::X, "Close (Esc)", true, false).clicked();
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                ui.add(egui::Label::new(RichText::new(item.title()).heading()).wrap());
            });
        });
    });
    ui.add_space(8.0);
    let o = &item.occurrence;
    let when = if o.all_day {
        let (first, last) = item.days();
        if first == last { first.format("%A %-d %B %Y").to_string() } else { format!("{} – {}", first.format("%a %-d %b"), last.format("%a %-d %b %Y")) }
    } else {
        format!("{}  ·  {}", o.start.with_timezone(&Local).format("%A %-d %B %Y"), item.hours())
    };
    views::note(ui, Icon::Calendar, t.text, &when, "");
    // The organiser's zone, when its clock says another time.
    if let Some(zone) = event.start.as_ref().and_then(|s| s.tzid.as_deref()).and_then(ical::zone).filter(|_| !o.all_day) {
        let there = o.start.with_timezone(&zone);
        if there.offset().fix() != o.start.with_timezone(&Local).offset().fix() {
            views::note(ui, Icon::Calendar, t.text_dim, &format!("{} in {}", there.format("%H:%M"), zone.name().replace('_', " ")), "");
        }
    }
    if !event.location.is_empty() {
        url = place(ui, t, &event.location);
    }
    if let Some(call) = call_link(event)
        && ui.button("Join call").on_hover_text(format!("Open the call on {}", crate::html::host(&call))).clicked()
    {
        url = Some(views::LinkInfo::new(ui.ctx(), &call, &call).clean);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(3), item.color);
        ui.label(RichText::new(calendar).size(13.0).color(t.text_dim));
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let (edit, delete) = can;
        let button = |icon: Icon, text: &str, color| egui::Button::image_and_text(icon.image(icons::SIZE, color), RichText::new(text).color(color));
        if ui.add_enabled(edit.is_ok(), button(Icon::Pencil, "Edit", t.text)).on_disabled_hover_text(edit.err().unwrap_or_default()).clicked() {
            *act = Some(EventAction::Edit);
        }
        if ui.add_enabled(delete.is_ok(), button(Icon::Trash, "Delete", t.danger)).on_disabled_hover_text(delete.err().unwrap_or_default()).clicked() {
            *act = Some(EventAction::Delete);
        }
    });
    match event.status.as_str() {
        "CANCELLED" => views::note(ui, Icon::Ban, t.danger, "Cancelled by the organiser", ""),
        "TENTATIVE" => views::note(ui, Icon::Spam, t.text_dim, "Tentative: not confirmed yet", ""),
        _ => {}
    }
    if let Some(organizer) = &event.organizer {
        ui.add_space(12.0);
        ui.label(RichText::new("Organiser").size(13.0).color(t.text_dim));
        ui.label(person(organizer));
    }
    if !event.attendees.is_empty() {
        ui.add_space(12.0);
        ui.label(RichText::new(format!("{} invited", event.attendees.len())).size(13.0).color(t.text_dim));
        for attendee in &event.attendees {
            let answer = match attendee.partstat.as_str() {
                "ACCEPTED" => "accepted",
                "DECLINED" => "declined",
                "TENTATIVE" => "maybe",
                "DELEGATED" => "delegated",
                _ => "not answered",
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(person(attendee));
                ui.label(RichText::new(answer).size(13.0).color(t.text_dim));
            });
        }
    }
    if !event.description.trim().is_empty() {
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        // Text from the event is untrusted, as a message is: plain, with its links cleaned.
        url = ui.scope(|ui| views::body(ui, t, &crate::html::plain_blocks(event.description.trim()))).inner;
    }
    url
}

/// When an event happens, in one line: its dates for an all-day one, else its day and hours here;
/// "repeats" when it does.
pub fn event_when(event: &ical::Event) -> String {
    let Some(start) = event.start.as_ref() else { return String::new() };
    let Some(first) = start.utc() else { return String::new() };
    let end = event.end.as_ref().and_then(ical::When::utc).or_else(|| event.duration.map(|d| first + Duration::seconds(d)));
    let text = if start.date_only {
        let last = end.map_or(first, |e| (e - Duration::seconds(1)).max(first));
        if last.date_naive() == first.date_naive() { first.format("%A %-d %B %Y").to_string() } else { format!("{} – {}", first.format("%a %-d %b"), last.format("%a %-d %b %Y")) }
    } else {
        let here = first.with_timezone(&Local);
        match end.map(|e| e.with_timezone(&Local)) {
            Some(e) if e.date_naive() == here.date_naive() => format!("{}  ·  {} – {}", here.format("%A %-d %B %Y"), here.format("%H:%M"), e.format("%H:%M")),
            Some(e) if e > here => format!("{} – {}", here.format("%a %-d %b %H:%M"), e.format("%a %-d %b %Y %H:%M")),
            _ => here.format("%A %-d %B %Y  ·  %H:%M").to_string(),
        }
    };
    if event.rules.is_empty() { text } else { format!("{text}  ·  repeats") }
}

/// What the calendars here say about an invitation, for its card.
#[derive(Clone, Debug, Default)]
pub struct InviteInfo {
    /// Events of the calendars shown at the same time.
    pub conflicts: Vec<String>,
    /// The answer given already, from the copy in a calendar here.
    pub answered: Option<String>,
    /// A calendar here has it.
    pub in_calendar: bool,
    /// An answer sent by the user (their own copy, in Sent).
    pub mine: bool,
}

/// What was chosen on an invitation's card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invite {
    /// `ACCEPTED`, `TENTATIVE` or `DECLINED`.
    Answer(&'static str),
    Remove,
    Add,
    /// A link of the event (its place, or the call to join), cleaned of tracking.
    Open(String),
    /// The event in the Calendar section, with its details open.
    Show,
}

/// Hosts of video calls: their link in an event is the call to join.
const CALLS: [&str; 9] = ["meet.google.com", "zoom.us", "teams.microsoft.com", "teams.live.com", "whereby.com", "webex.com", "meet.jit.si", "gotomeeting.com", "facetime.apple.com"];

/// The web links written in a text, as they are.
fn links(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || c == '<' || c == '>').filter(|w| w.starts_with("https://") || w.starts_with("http://")).map(|w| w.trim_end_matches([',', '.', ';', ')']))
}

/// The video call of an event: a link to a call service in its place, else in its description.
pub fn call_link(event: &ical::Event) -> Option<String> {
    let is_call = |url: &&str| CALLS.iter().any(|h| crate::html::host(url) == *h || crate::html::host(url).ends_with(&format!(".{h}")));
    links(&event.location).find(is_call).or_else(|| links(&event.description).find(is_call)).map(str::to_owned)
}

/// An event's place, as a link when it is one (a call, a map), with where it really leads on hover.
/// Returns the link clicked, cleaned of tracking.
fn place(ui: &mut egui::Ui, t: &Tokens, location: &str) -> Option<String> {
    let Some(url) = links(location).next() else {
        views::note(ui, Icon::MapPin, t.text_dim, location, "");
        return None;
    };
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.add(Icon::MapPin.image(icons::SIZE, t.text_dim));
        let link = views::LinkInfo::new(ui.ctx(), location, url);
        let tint = if link.misleading { t.danger } else { t.accent };
        if ui.add(egui::Link::new(RichText::new(location).size(13.0).color(tint))).on_hover_text(&link.tip).clicked() {
            clicked = Some(link.clean);
        }
    });
    clicked
}

/// The card above a message carrying a calendar part: an invitation with Accept, Maybe and Decline; a
/// cancellation with Remove from calendar; someone's answer; an event to add. `answerable`: why not, for a
/// message held as suspicious (no buttons then).
pub fn invitation_card(ui: &mut egui::Ui, t: &Tokens, calendar: &ical::Calendar, info: &InviteInfo, answerable: Result<(), &str>) -> Option<Invite> {
    let event = calendar.events.iter().find(|e| e.recurrence_id.is_none()).or(calendar.events.first())?;
    let mut chosen = None;
    let title = if event.summary.trim().is_empty() { "(no title)" } else { event.summary.trim() };
    let kind = match calendar.method.as_str() {
        "REQUEST" if info.in_calendar => "Updated invitation".to_owned(),
        "REQUEST" => "Invitation".to_owned(),
        "CANCEL" => "Cancelled by the organiser".to_owned(),
        "REPLY" if info.mine => match event.attendees.first().map(|a| a.partstat.as_str()) {
            Some("ACCEPTED") => "You accepted this invitation".to_owned(),
            Some("DECLINED") => "You declined this invitation".to_owned(),
            Some("TENTATIVE") => "You answered maybe to this invitation".to_owned(),
            _ => "Your answer to this invitation".to_owned(),
        },
        "REPLY" => {
            let who = event.attendees.first().map(person).unwrap_or_default();
            let answer = match event.attendees.first().map(|a| a.partstat.as_str()) {
                Some("ACCEPTED") => "accepted",
                Some("DECLINED") => "declined",
                Some("TENTATIVE") => "answered maybe to",
                _ => "answered",
            };
            format!("{who} {answer} your invitation")
        }
        _ => "Event".to_owned(),
    };
    egui::Frame::new().fill(t.input).stroke(Stroke::new(1.0, t.border)).corner_radius(CornerRadius::same(theme::CONTROL_RADIUS)).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let color = if calendar.method == "CANCEL" { t.danger } else { t.accent };
        views::note(ui, Icon::Calendar, color, &kind, "");
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(title).size(theme::SUBTITLE).family(theme::bold()).color(t.text)).wrap());
        views::note(ui, Icon::Calendar, t.text_dim, &event_when(event), "");
        if !event.location.is_empty()
            && let Some(url) = place(ui, t, &event.location)
        {
            chosen = Some(Invite::Open(url));
        }
        if let Some(organizer) = &event.organizer {
            let invited = if event.attendees.len() > 1 { format!("  ·  {} invited", event.attendees.len()) } else { String::new() };
            ui.label(RichText::new(format!("Organiser: {}{invited}", person(organizer))).size(13.0).color(t.text_dim));
        }
        if !info.conflicts.is_empty() && calendar.method == "REQUEST" {
            views::note(ui, Icon::Spam, t.danger, &format!("Busy then: {}", info.conflicts.join(", ")), "");
        }
        // The answer given, and the event in the calendar (a declined one is out of the views).
        let said = info.answered.as_deref().map(|answer| match answer {
            "ACCEPTED" => "You accepted",
            "DECLINED" => "You declined",
            "TENTATIVE" => "You answered maybe",
            _ => "In your calendar, not answered yet",
        });
        let viewable = info.in_calendar && info.answered.as_deref() != Some("DECLINED") && calendar.method != "CANCEL";
        if said.is_some() || viewable {
            ui.horizontal(|ui| {
                if let Some(said) = said {
                    ui.label(RichText::new(said).size(13.0).color(t.text_dim));
                }
                if viewable && ui.link(RichText::new("View in calendar").size(13.0)).on_hover_text("Open it in Calendar, with its details").clicked() {
                    chosen = Some(Invite::Show);
                }
            });
        }
        ui.add_space(8.0);
        match answerable {
            Err(why) if calendar.method != "REPLY" => {
                ui.add(egui::Label::new(RichText::new(why).size(13.0).color(t.danger)).wrap());
            }
            _ => {
                ui.horizontal(|ui| {
                    match calendar.method.as_str() {
                        "REQUEST" => {
                            // Answered: that answer shows as chosen, the others change it.
                            let given = info.answered.as_deref().filter(|a| ["ACCEPTED", "TENTATIVE", "DECLINED"].contains(a));
                            let answers = [
                                ("Accept", "ACCEPTED", "Tell the organiser, and add it to your calendar"),
                                ("Maybe", "TENTATIVE", "Tell the organiser you may come; it goes in your calendar as tentative"),
                                ("Decline", "DECLINED", "Tell the organiser you will not come; it leaves your calendar views"),
                            ];
                            for (label, partstat, tip) in answers {
                                let response = match given {
                                    Some(g) if g == partstat => ui.add_enabled(false, egui::Button::new(RichText::new(label).color(t.on_accent)).fill(t.accent)).on_disabled_hover_text("Your answer"),
                                    Some(_) => ui.button(label).on_hover_text(format!("Change your answer: {}", tip.to_lowercase())),
                                    None if partstat == "ACCEPTED" => views::primary_button(ui, t, label, true).on_hover_text(tip),
                                    None => ui.button(label).on_hover_text(tip),
                                };
                                if response.clicked() {
                                    chosen = Some(Invite::Answer(partstat));
                                }
                            }
                        }
                        "CANCEL" if info.in_calendar && ui.button("Remove from calendar").clicked() => chosen = Some(Invite::Remove),
                        "PUBLISH" | "" if !info.in_calendar && views::primary_button(ui, t, "Add to calendar", true).clicked() => chosen = Some(Invite::Add),
                        _ => {}
                    }
                    // The call to join, beside the answers; not on a cancelled event, nor on a message
                    // held as suspicious (its link could be anything).
                    if let Some(call) = call_link(event).filter(|_| calendar.method != "CANCEL") {
                        ui.add_space(8.0);
                        if ui.button("Join call").on_hover_text(format!("Open the call on {}", crate::html::host(&call))).clicked() {
                            chosen = Some(Invite::Open(views::LinkInfo::new(ui.ctx(), &call, &call).clean));
                        }
                    }
                });
            }
        }
    });
    chosen
}

fn person(p: &ical::Person) -> String {
    if p.name.is_empty() || p.name.eq_ignore_ascii_case(&p.email) { p.email.clone() } else { format!("{} <{}>", p.name, p.email) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(start: &str, end: &str, all_day: bool) -> Item {
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        Item { occurrence: Occurrence { uid: "u".into(), start: at(start), end: at(end), all_day, event: ical::Event { summary: "Review".into(), ..Default::default() } }, calendar: String::new(), color: Color32::WHITE }
    }

    #[test]
    fn an_all_day_event_covers_its_dates_and_views_have_their_ranges() {
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let two_days = item("2026-10-12T00:00:00Z", "2026-10-14T00:00:00Z", true);
        assert_eq!(two_days.days(), (day(12), day(13)), "the end date is the day after");
        assert!(two_days.on(day(13)) && !two_days.on(day(14)));
        assert_eq!(range(View::Week, day(21)), (day(19), 7));
        assert_eq!(range(View::Month, day(21)), (NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(), 42), "from the Monday before the 1st");
        assert_eq!(title(View::Week, day(21)), ("Oct 19 – 25, 2026".to_owned(), "W43".to_owned()));
    }

    #[test]
    fn every_view_and_the_details_lay_out_with_events() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let monday = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let mut long = item("2026-10-13T09:00:00Z", "2026-10-13T10:30:00Z", false);
        long.occurrence.event = ical::Event { summary: "Plan review".into(), location: "Room 2".into(), description: "See https://example.org/plan".into(), attendees: vec![ical::Person { email: "bob@example.org".into(), name: "Bob".into(), partstat: "ACCEPTED".into() }], ..Default::default() };
        let items = vec![item("2026-10-12T00:00:00Z", "2026-10-14T00:00:00Z", true), long, item("2026-10-13T09:30:00Z", "2026-10-13T11:00:00Z", false)];
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1280.0, 800.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                let t = Tokens::dark();
                assert!(week(ui, &t, monday, &items, monday, true).is_none());
                assert!(month(ui, &t, monday, 10, &items, monday).is_none());
                assert!(agenda(ui, &t, monday, 30, &items, monday).is_none());
                let mut close = false;
                assert!(details(ui, &t, &items[1], "Work", (Ok(()), Ok(())), &mut close, &mut None).is_none() && !close);
                let invite = ical::parse("BEGIN:VCALENDAR\nMETHOD:REQUEST\nBEGIN:VEVENT\nUID:i\nSUMMARY:Plan review\nDTSTART:20261013T090000Z\nDTEND:20261013T100000Z\nORGANIZER;CN=Ada:mailto:ada@example.org\nATTENDEE:mailto:bob@example.org\nEND:VEVENT\nEND:VCALENDAR\n");
                let info = InviteInfo { conflicts: vec!["Standup".into()], ..Default::default() };
                assert!(invitation_card(ui, &t, &invite, &info, Ok(())).is_none());
                assert!(invitation_card(ui, &t, &invite, &info, Err("Answering is off")).is_none());
                let answered = InviteInfo { answered: Some("ACCEPTED".into()), in_calendar: true, ..Default::default() };
                assert!(invitation_card(ui, &t, &invite, &answered, Ok(())).is_none(), "the answer given shows as chosen");
                let cancel = ical::Calendar { method: "CANCEL".into(), ..invite.clone() };
                assert!(invitation_card(ui, &t, &cancel, &InviteInfo { in_calendar: true, ..Default::default() }, Ok(())).is_none());
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn a_call_is_found_in_the_place_or_the_description() {
        let event = |location: &str, description: &str| ical::Event { location: location.into(), description: description.into(), ..Default::default() };
        assert_eq!(call_link(&event("https://meet.google.com/abc-defg-hij", "")).as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        assert_eq!(call_link(&event("Room 2", "Join: <https://acme.zoom.us/j/123?pwd=x>.")).as_deref(), Some("https://acme.zoom.us/j/123?pwd=x"));
        assert_eq!(call_link(&event("https://maps.example.org/room-2", "")), None, "a map is not a call");
        assert_eq!(call_link(&event("", "https://evil-zoom.us.example.com/j/1")), None, "only the service's own host");
    }

    #[test]
        fn overlapping_events_share_a_day_side_by_side() {
        let (a, b, c) = (item("2026-10-12T09:00:00Z", "2026-10-12T10:00:00Z", false), item("2026-10-12T09:30:00Z", "2026-10-12T10:30:00Z", false), item("2026-10-12T11:00:00Z", "2026-10-12T12:00:00Z", false));
        let placed = lanes(&[(0, &a, 540.0, 600.0), (1, &b, 570.0, 630.0), (2, &c, 660.0, 720.0)]);
        let shape: Vec<(usize, usize, usize)> = placed.iter().map(|p| (p.0, p.4, p.5)).collect();
        assert_eq!(shape, [(0, 0, 2), (1, 1, 2), (2, 0, 1)], "two side by side, the later one alone");
    }
}
