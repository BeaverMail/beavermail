// SPDX-License-Identifier: AGPL-3.0-only

//! The search put together from fields: it reads the operators in the search box and writes them
//! back there, so the box stays the one place a search lives, and its words teach the operators.

use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::theme::{self, Tokens};

const FIELD: f32 = 280.0;

/// The fields of the form, as typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    pub from: String,
    pub to: String,
    pub subject: String,
    /// Everything else of the query: words, phrases, `in:` and operators asked twice.
    pub words: String,
    /// A day, a month or a year: "2026-09-01", "2026-09", "2026".
    pub after: String,
    pub before: String,
    pub attachment: bool,
    pub unread: bool,
}

impl Fields {
    /// The fields `query` fills; what has no field of its own stays in the words.
    pub fn parse(query: &str) -> Self {
        let mut fields = Self::default();
        let mut words = Vec::new();
        for token in beavermail_store::tokens(query) {
            let slot = match token.split_once(':') {
                Some(("from", v)) if fields.from.is_empty() && !v.is_empty() => Some((&mut fields.from, v)),
                Some(("to", v)) if fields.to.is_empty() && !v.is_empty() => Some((&mut fields.to, v)),
                Some(("subject", v)) if fields.subject.is_empty() && !v.is_empty() => Some((&mut fields.subject, v)),
                Some(("after", v)) if fields.after.is_empty() && day(v) => Some((&mut fields.after, v)),
                Some(("before", v)) if fields.before.is_empty() && day(v) => Some((&mut fields.before, v)),
                Some(("has", v)) if v.eq_ignore_ascii_case("attachment") => {
                    fields.attachment = true;
                    continue;
                }
                Some(("is", v)) if v.eq_ignore_ascii_case("unread") => {
                    fields.unread = true;
                    continue;
                }
                _ => None,
            };
            match slot {
                Some((field, value)) => *field = value.to_owned(),
                None => words.push(quoted(&token)),
            }
        }
        fields.words = words.join(" ");
        fields
    }

    /// The query the fields ask for, as typed in the search box.
    pub fn query(&self) -> String {
        // The fields first: an operator asked twice stays in the words, after the one the field shows.
        let mut parts: Vec<String> = Vec::new();
        for (operator, value) in [("from", &self.from), ("to", &self.to), ("subject", &self.subject), ("after", &self.after), ("before", &self.before)] {
            if !value.trim().is_empty() {
                parts.push(format!("{operator}:{}", quoted(value.trim())));
            }
        }
        if self.attachment {
            parts.push("has:attachment".into());
        }
        if self.unread {
            parts.push("is:unread".into());
        }
        if !self.words.trim().is_empty() {
            parts.push(self.words.trim().to_owned());
        }
        parts.join(" ")
    }

    /// Why a date cannot be searched for, if one cannot.
    fn date_problem(&self) -> Option<&'static str> {
        [&self.after, &self.before].into_iter().any(|d| !d.trim().is_empty() && !day(d.trim())).then_some("Write a date as 2026-09-01, 2026-09 or 2026")
    }
}

/// A day, a month or a year, as `after:` and `before:` take them.
fn day(text: &str) -> bool {
    let full = match text.matches('-').count() {
        0 => format!("{text}-01-01"),
        1 => format!("{text}-01"),
        _ => text.to_owned(),
    };
    chrono::NaiveDate::parse_from_str(&full, "%Y-%m-%d").is_ok()
}

/// A value that stays one word in the search box: in quotes when it holds spaces.
fn quoted(value: &str) -> String {
    match value.split_once(':') {
        Some((operator, v)) if v.contains(char::is_whitespace) => format!("{operator}:\"{v}\""),
        _ if value.contains(char::is_whitespace) => format!("\"{value}\""),
        _ => value.to_owned(),
    }
}

/// The form, under the search box. True when Search is chosen (or Enter pressed in a field) with
/// fields that can be searched for.
pub fn panel(ui: &mut egui::Ui, t: &Tokens, fields: &mut Fields) -> bool {
    let mut enter = false;
    let mut row = |ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str| {
        ui.label(label);
        let response = ui.add(crate::sized_field(text, FIELD).hint_text(hint));
        enter |= response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.end_row();
    };
    egui::Grid::new("search_form").num_columns(2).spacing([16.0, 10.0]).min_col_width(84.0).show(ui, |ui| {
        row(ui, "From", &mut fields.from, "Name or address");
        row(ui, "To", &mut fields.to, "Name or address, also in Cc");
        row(ui, "Subject", &mut fields.subject, "Words in the subject");
        row(ui, "Has the words", &mut fields.words, "Anywhere in the message");
        row(ui, "After", &mut fields.after, "2026-09-01, 2026-09 or 2026");
        row(ui, "Before", &mut fields.before, "Up to the day before this one");
    });
    ui.add_space(4.0);
    crate::views::switch_row(ui, t, &mut fields.attachment, "Has an attachment", "");
    crate::views::switch_row(ui, t, &mut fields.unread, "Unread only", "");
    let problem = fields.date_problem();
    if let Some(problem) = problem {
        ui.label(RichText::new(problem).size(13.0).color(t.danger));
    }
    ui.add_space(8.0);
    let mut search = false;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
        let why = problem.or_else(|| fields.query().is_empty().then_some("Fill in at least one field"));
        search = crate::views::primary_button(ui, t, "Search", why.is_none()).on_disabled_hover_text(why.unwrap_or_default()).clicked();
        if ui.button("Clear").clicked() {
            *fields = Fields::default();
        }
    });
    (search || enter) && problem.is_none() && !fields.query().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_box_and_the_form_say_the_same_search() {
        let fields = Fields::parse(r#"budget from:"Ada Lovelace" in:inbox has:attachment after:2026-09 from:bob"#);
        assert_eq!(fields.from, "Ada Lovelace");
        assert_eq!(fields.after, "2026-09");
        assert!(fields.attachment && !fields.unread);
        assert_eq!(fields.words, "budget in:inbox from:bob", "what has no field of its own stays");
        assert_eq!(fields.query(), r#"from:"Ada Lovelace" after:2026-09 has:attachment budget in:inbox from:bob"#);
        assert_eq!(Fields::parse(&fields.query()), fields, "read back as written");
        let typed = Fields { subject: "q3 plan".into(), unread: true, ..Fields::default() };
        assert_eq!(typed.query(), r#"subject:"q3 plan" is:unread"#);
        assert_eq!(Fields::parse("after:soon").words, "after:soon", "not a date: left as typed");
        assert!(Fields { before: "2026-13".into(), ..Fields::default() }.date_problem().is_some());
        assert_eq!(Fields { after: "2026".into(), ..Fields::default() }.date_problem(), None);
    }
}
