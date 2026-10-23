// SPDX-License-Identifier: AGPL-3.0-only

//! The dialog to add an event or change one of the user's own: title, when (all day or from a time to
//! a time), where, which calendar and notes. Times are this computer's; they are written in UTC.

use beavermail_carddav::ical::{Draft, When};
use chrono::{Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::calview::Item;
use crate::theme::{self, Tokens};
use crate::views;

/// What the dialog holds while it is open.
#[derive(Clone, Debug)]
pub struct EventForm {
    /// The event changed, or `None` for a new one.
    pub uid: Option<String>,
    /// The calendar it goes to (by URL); chosen only for a new event.
    pub calendar: String,
    pub title: String,
    pub location: String,
    pub notes: String,
    pub all_day: bool,
    /// Here, on this computer's clock. An all-day event takes their dates, its end the last day.
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// The months the two date pickers show.
    months: (NaiveDate, NaiveDate),
    focus_first: bool,
}

impl EventForm {
    /// A new event on `day`, an hour from `minutes` past midnight, in `calendar`.
    pub fn new(day: NaiveDate, minutes: u32, calendar: &str) -> Self {
        let start = day.and_time(NaiveTime::from_num_seconds_from_midnight_opt(minutes.min(23 * 60) * 60, 0).unwrap_or_default());
        Self::with(None, calendar, start, start + Duration::hours(1))
    }

    /// The event of `item`, to change: its values as the views show them.
    pub fn editing(item: &Item) -> Self {
        let o = &item.occurrence;
        let (start, end) = if o.all_day {
            // Its dates, not this computer's day: the end is the day after its last.
            let first = o.start.date_naive();
            (first.and_time(NaiveTime::MIN), (o.end - Duration::days(1)).date_naive().max(first).and_time(NaiveTime::MIN))
        } else {
            (o.start.with_timezone(&Local).naive_local(), o.end.with_timezone(&Local).naive_local())
        };
        let mut form = Self::with(Some(o.uid.clone()), &item.calendar, start, end);
        let event = &o.event;
        (form.title, form.location, form.notes, form.all_day) = (event.summary.clone(), event.location.clone(), event.description.clone(), o.all_day);
        form
    }

    fn with(uid: Option<String>, calendar: &str, start: NaiveDateTime, end: NaiveDateTime) -> Self {
        let month = |t: NaiveDateTime| t.date() - Duration::days(i64::from(chrono::Datelike::day0(&t.date())));
        Self { uid, calendar: calendar.to_owned(), title: String::new(), location: String::new(), notes: String::new(), all_day: false, start, end, months: (month(start), month(end)), focus_first: true }
    }

    /// What is written for it, or why it cannot be.
    pub fn draft(&self) -> Result<Draft, &'static str> {
        let (start, end) = if self.all_day {
            if self.end.date() < self.start.date() {
                return Err("It ends before it starts");
            }
            let date = |d: NaiveDate| When { value: d.format("%Y%m%d").to_string(), tzid: None, date_only: true };
            (date(self.start.date()), date(self.end.date() + Duration::days(1)))
        } else {
            if self.end <= self.start {
                return Err("It ends before it starts");
            }
            let utc = |t: NaiveDateTime| Local.from_local_datetime(&t).earliest().map(|t| When { value: t.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string(), tzid: None, date_only: false });
            (utc(self.start).ok_or("That start does not exist here (a change of clock)")?, utc(self.end).ok_or("That end does not exist here (a change of clock)")?)
        };
        Ok(Draft { summary: self.title.trim().to_owned(), location: self.location.trim().to_owned(), description: self.notes.trim().to_owned(), start, end })
    }
}

/// The calendars a new event can go to, as (URL, name).
pub type Targets<'a> = &'a [(String, String)];

/// The dialog. Returns the form and what to write when the user saves; closes on Cancel and Esc.
pub fn dialog(ctx: &egui::Context, t: &Tokens, state: &mut Option<EventForm>, targets: Targets) -> Option<(EventForm, Draft)> {
    let form = state.as_mut()?;
    let today = Local::now().date_naive();
    let (mut saved, mut close) = (None, false);
    let modal = egui::Modal::new(egui::Id::new("event_form")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        ui.heading(if form.uid.is_some() { "Change event" } else { "New event" });
        ui.add_space(12.0);
        let length = form.end - form.start;
        let start_before = form.start;
        crate::form_grid(ui, "event_form_grid", |ui| {
            ui.label("Title");
            let title = ui.add(crate::field(&mut form.title).hint_text("(no title)"));
            if std::mem::take(&mut form.focus_first) {
                title.request_focus();
            }
            ui.end_row();
            ui.label("Starts");
            moment(ui, t, "start", &mut form.start, &mut form.months.0, form.all_day, today);
            ui.end_row();
            ui.label("Ends");
            moment(ui, t, "end", &mut form.end, &mut form.months.1, form.all_day, today);
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut form.all_day, "All day");
            ui.end_row();
            ui.label("Location");
            ui.add(crate::field(&mut form.location).hint_text("A place or a link"));
            ui.end_row();
            ui.label("Calendar");
            if form.uid.is_some() || targets.len() < 2 {
                let name = targets.iter().find(|(url, _)| *url == form.calendar).map_or("", |(_, name)| name.as_str());
                ui.label(RichText::new(name).color(t.text_dim));
            } else {
                let name = targets.iter().find(|(url, _)| *url == form.calendar).map_or("", |(_, name)| name.as_str()).to_owned();
                egui::ComboBox::from_id_salt("event_calendar").width(crate::FIELD_WIDTH).selected_text(name).show_ui(ui, |ui| {
                    for (url, name) in targets {
                        ui.selectable_value(&mut form.calendar, url.clone(), name);
                    }
                });
            }
            ui.end_row();
        });
        // A new start takes the end along, so the event keeps its length.
        if form.start != start_before {
            form.end = form.start + length;
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Notes").color(t.text_dim));
        ui.add(egui::TextEdit::multiline(&mut form.notes).id_salt("event_notes").desired_rows(3).desired_width(f32::INFINITY).margin(egui::Margin::same(12)));
        let draft = form.draft();
        if let Err(why) = draft {
            ui.add_space(8.0);
            views::note(ui, crate::icons::Icon::Ban, t.danger, why, "");
        }
        ui.add_space(16.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let label = if form.uid.is_some() { "Save changes" } else { "Add event" };
            let why = draft.as_ref().err().copied().unwrap_or_default();
            // Enter in a one-line field saves; in the notes it starts a new line.
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && !ui.ctx().memory(|m| m.has_focus(egui::Id::new("event_notes")));
            if views::primary_button(ui, t, label, draft.is_ok()).on_disabled_hover_text(why).clicked() || (enter && draft.is_ok()) {
                saved = draft.ok();
            }
            close = ui.button("Cancel").clicked();
        });
    });
    if let Some(draft) = saved {
        return state.take().map(|form| (form, draft));
    }
    if close || modal.should_close() {
        *state = None;
    }
    None
}

/// A day, picked from a month, and a time in quarter hours unless the event lasts all day.
fn moment(ui: &mut egui::Ui, t: &Tokens, id: &str, at: &mut NaiveDateTime, month: &mut NaiveDate, all_day: bool, today: NaiveDate) {
    ui.horizontal(|ui| {
        let width = if all_day { crate::FIELD_WIDTH } else { crate::FIELD_WIDTH - 118.0 };
        let button = ui.add_sized([width, theme::CONTROL_H], egui::Button::new(at.format("%a %-d %b %Y").to_string()));
        if button.clicked() {
            *month = at.date() - Duration::days(i64::from(chrono::Datelike::day0(&at.date())));
        }
        egui::Popup::from_toggle_button_response(&button).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            let mut day = at.date();
            views::calendar(ui, t, &mut day, month, today, None);
            if day != at.date() {
                *at = day.and_time(at.time());
                ui.close();
            }
        });
        if !all_day {
            let minutes = at.num_seconds_from_midnight() / 60;
            egui::ComboBox::from_id_salt(("event_time", id)).width(110.0).selected_text(at.format("%H:%M").to_string()).height(320.0).show_ui(ui, |ui| {
                for m in (0..24 * 60).step_by(15) {
                    if ui.selectable_label(m == minutes, format!("{:02}:{:02}", m / 60, m % 60)).clicked() {
                        *at = at.date().and_time(NaiveTime::from_num_seconds_from_midnight_opt(m * 60, 0).unwrap_or_default());
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(d: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn a_draft_has_its_times_in_utc_or_its_dates_and_refuses_an_end_before_the_start() {
        let mut form = EventForm::new(at(12, 0, 0).date(), 9 * 60, "local:");
        assert_eq!((form.start, form.end), (at(12, 9, 0), at(12, 10, 0)), "an hour from the time asked");
        let draft = form.draft().unwrap();
        let utc = |t: NaiveDateTime| Local.from_local_datetime(&t).earliest().unwrap().with_timezone(&Utc);
        assert_eq!(draft.start.utc(), Some(utc(at(12, 9, 0))));
        assert!(!draft.start.date_only && draft.start.value.ends_with('Z'));
        form.all_day = true;
        form.end = at(13, 0, 0);
        let draft = form.draft().unwrap();
        assert_eq!((draft.start.value.as_str(), draft.end.value.as_str(), draft.end.date_only), ("20261012", "20261014", true), "two days: the end is the day after the last");
        form.all_day = false;
        form.end = at(12, 8, 0);
        assert!(form.draft().is_err());
    }

    #[test]
    fn the_dialog_is_as_wide_as_a_form_and_saves_nothing_until_asked() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))), ..Default::default() };
        let mut state = Some(EventForm::new(at(12, 0, 0).date(), 9 * 60, "local:"));
        let targets = [("local:".to_owned(), "On this computer".to_owned()), ("https://dav.example.org/work/".to_owned(), "Work".to_owned())];
        // Two passes: the first one measures.
        for _ in 0..2 {
            let mut output = ctx.run_ui(input(), |ui| assert!(dialog(ui.ctx(), &Tokens::dark(), &mut state, &targets).is_none()));
            output.textures_delta.clear(); // nothing is drawn here
        }
        let rect = ctx.memory(|m| m.area_rect(egui::Id::new("event_form"))).expect("the dialog is shown");
        assert!(rect.width() < crate::FORM_WIDTH + 80.0 && rect.height() < 640.0, "{rect:?}");
        assert!(state.is_some(), "still open");
    }
}
