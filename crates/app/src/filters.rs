// SPDX-License-Identifier: AGPL-3.0-only

//! Filters: rules the user sets for new mail in an account's Inbox, applied by this app when the mail
//! arrives (not by the server: they run while BeaverMail is open, and mail that came meanwhile is
//! sorted at the next start).

use beavermail_core::{Condition, Filter, FilterAction, MessageSummary};
use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::actions::Action;
use crate::theme::{self, Tokens};

/// Whether `filter` holds for message `m`, whose To and Cc addresses are `recipients`. A filter
/// without conditions holds for nothing, so an unfinished one never takes all the mail.
pub(crate) fn matches(filter: &Filter, m: &MessageSummary, recipients: &str) -> bool {
    let has = |field: &str, text: &str| !text.trim().is_empty() && field.to_lowercase().contains(&text.trim().to_lowercase());
    let holds = |c: &Condition| match c {
        Condition::From(text) => has(&m.from, text) || has(&m.address, text),
        Condition::To(text) => has(recipients, text),
        Condition::Subject(text) => has(&m.subject, text),
        Condition::Attachment => m.has_attachments,
    };
    match filter.conditions.as_slice() {
        [] => false,
        all if filter.all => all.iter().all(holds),
        any => any.iter().any(holds),
    }
}

/// What the enabled `filters` do to message `m`, in the order to send them: changes to its flags,
/// then at most one move, the first one asked for. A filter that moves the message, or says to stop,
/// is the last one tried.
pub(crate) fn actions(filters: &[Filter], m: &MessageSummary, recipients: &str) -> Vec<Action> {
    let (mut flags, mut moved): (Vec<Action>, Option<Action>) = (Vec::new(), None);
    for filter in filters.iter().filter(|f| f.enabled && matches(f, m, recipients)) {
        for action in &filter.actions {
            let flag = match action {
                FilterAction::Read => Action::Unread(false),
                FilterAction::Star => Action::Star(true),
                FilterAction::Label(n) => Action::Label(*n, true),
                FilterAction::Move(folder) => {
                    moved = moved.or(Some(Action::Move(folder.clone())));
                    continue;
                }
                FilterAction::Trash => {
                    moved = moved.or(Some(Action::Trash));
                    continue;
                }
            };
            if !flags.contains(&flag) {
                flags.push(flag);
            }
        }
        if moved.is_some() || filter.stop {
            break;
        }
    }
    flags.extend(moved);
    flags
}

/// The filter in one line, for the list: "From has ada · Subject has invoice → Move to Receipts".
pub(crate) fn describe(filter: &Filter, folder_title: impl Fn(&str) -> String) -> String {
    let conditions: Vec<String> = filter
        .conditions
        .iter()
        .map(|c| match c {
            Condition::From(t) => format!("From has {t}"),
            Condition::To(t) => format!("To has {t}"),
            Condition::Subject(t) => format!("Subject has {t}"),
            Condition::Attachment => "Has an attachment".to_owned(),
        })
        .collect();
    let actions: Vec<String> = filter
        .actions
        .iter()
        .map(|a| match a {
            FilterAction::Move(folder) => format!("Move to {}", folder_title(folder)),
            FilterAction::Read => "Mark as read".to_owned(),
            FilterAction::Star => "Star".to_owned(),
            FilterAction::Label(n) => format!("Label {}", crate::theme::label(&format!("$label{n}")).0),
            FilterAction::Trash => "Move to Trash".to_owned(),
        })
        .collect();
    let joiner = if filter.all { " and " } else { " or " };
    format!("{} → {}", if conditions.is_empty() { "Nothing yet".to_owned() } else { conditions.join(joiner) }, if actions.is_empty() { "nothing".to_owned() } else { actions.join(", ") })
}

/// What the user did on the Filters page.
pub(crate) enum PageAction {
    /// Show the filters of another account, by its key.
    Account(String),
    New,
    Edit(usize),
    Toggle(usize),
    Delete(usize),
    /// Moved one place up (`true`) or down.
    Up(usize, bool),
    RunNow(usize),
}

/// The Filters page: the accounts to pick from as (key, address), the one shown, and every filter
/// with its place in the settings (the page shows those of `account`). `title` names a folder.
/// `running`: the filter Run now is applying; `busy`: the account's filters are running.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page(ui: &mut egui::Ui, t: &Tokens, accounts: &[(String, String)], account: &str, filters: &[Filter], title: &dyn Fn(&str) -> String, running: Option<usize>, busy: bool) -> Option<PageAction> {
    let mut action = None;
    if accounts.len() > 1 {
        ui.horizontal(|ui| {
            ui.label("Account");
            let shown = accounts.iter().find(|(k, _)| k == account).map_or("", |(_, a)| a.as_str());
            egui::ComboBox::from_id_salt("filters_account").width(crate::FIELD_WIDTH).selected_text(shown).show_ui(ui, |ui| {
                for (key, address) in accounts {
                    if ui.selectable_label(key == account, address).clicked() {
                        action = Some(PageAction::Account(key.clone()));
                    }
                }
            });
        });
        ui.add_space(8.0);
    }
    let hint = |ui: &mut egui::Ui, text: &str| ui.label(RichText::new(text).size(13.0).color(t.text_dim));
    hint(ui, "Applied in order to new mail in the Inbox while BeaverMail is open; mail that came meanwhile is sorted at the next start. Mail held as suspicious is left alone.");
    ui.add_space(8.0);
    let mine: Vec<usize> = (0..filters.len()).filter(|&i| filters[i].account == account).collect();
    if mine.is_empty() {
        hint(ui, "No filters yet. One can move mail from a sender to a folder, star it, label it or mark it as read.");
    }
    for (place, &at) in mine.iter().enumerate() {
        let filter = &filters[at];
        let mut on = filter.enabled;
        let name = if filter.name.trim().is_empty() { "Unnamed filter" } else { filter.name.trim() };
        if crate::views::switch_row(ui, t, &mut on, name, &describe(filter, title)).changed() {
            action = Some(PageAction::Toggle(at));
        }
        ui.horizontal(|ui| {
            if ui.button("Edit…").clicked() {
                action = Some(PageAction::Edit(at));
            }
            if running == Some(at) {
                crate::views::slow_spinner(ui, 16.0);
                ui.label(RichText::new("Running…").color(t.text_dim));
            } else {
                let why = "Wait for the filters running now";
                if ui.add_enabled(!busy, egui::Button::new("Run now")).on_hover_text("Apply it to all the mail in the Inbox").on_disabled_hover_text(why).clicked() {
                    action = Some(PageAction::RunNow(at));
                }
            }
            if crate::icons::button(ui, t, crate::icons::Icon::ChevronUp, "Move up", place > 0, false).clicked() {
                action = Some(PageAction::Up(at, true));
            }
            if crate::icons::button(ui, t, crate::icons::Icon::ChevronDown, "Move down", place + 1 < mine.len(), false).clicked() {
                action = Some(PageAction::Up(at, false));
            }
            if crate::icons::button(ui, t, crate::icons::Icon::Trash, "Delete filter", true, false).clicked() {
                action = Some(PageAction::Delete(at));
            }
        });
        ui.add_space(12.0);
    }
    if ui.button("New filter…").clicked() {
        action = Some(PageAction::New);
    }
    action
}

/// The filter being made or changed, in a dialog. `Some(true)` to save it, `Some(false)` to leave it.
/// `folders` are those it can move mail to, as (name on the server, as shown).
pub(crate) fn editor(ctx: &egui::Context, t: &Tokens, filter: &mut Filter, folders: &[(String, String)]) -> Option<bool> {
    let mut done = None;
    let modal = crate::views::modal(ctx, egui::Id::new("filter_edit")).show(ctx, |ui| {
        ui.set_width(WIDTH);
        ui.heading(if filter.name.trim().is_empty() { "New filter" } else { filter.name.trim() });
        ui.label(RichText::new("What new mail it looks for, and what it does with what it finds.").color(t.text_dim));
        ui.add_space(16.0);
        ui.label("Name");
        ui.add(crate::sized_field(&mut filter.name, WIDTH).hint_text("Receipts, Newsletters…"));

        section(ui, "When");
        ui.horizontal(|ui| {
            ui.label("Mail matches when");
            egui::ComboBox::from_id_salt("filter_all").selected_text(if filter.all { "every condition" } else { "any condition" }).show_ui(ui, |ui| {
                ui.selectable_value(&mut filter.all, true, "every condition");
                ui.selectable_value(&mut filter.all, false, "any condition");
            });
            ui.label("holds");
        });
        let mut remove = None;
        for (i, condition) in filter.conditions.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let kinds = [("From has", Condition::From(String::new())), ("To or Cc has", Condition::To(String::new())), ("Subject has", Condition::Subject(String::new())), ("Has an attachment", Condition::Attachment)];
                let shown = kinds.iter().find(|(_, k)| std::mem::discriminant(k) == std::mem::discriminant(condition)).map_or("", |(l, _)| l);
                egui::ComboBox::from_id_salt(("filter_condition", i)).width(KIND).selected_text(shown).show_ui(ui, |ui| {
                    for (label, kind) in kinds {
                        if ui.selectable_label(std::mem::discriminant(&kind) == std::mem::discriminant(condition), label).clicked() {
                            let text = condition_text(condition).to_owned();
                            *condition = match kind {
                                Condition::From(_) => Condition::From(text),
                                Condition::To(_) => Condition::To(text),
                                Condition::Subject(_) => Condition::Subject(text),
                                Condition::Attachment => Condition::Attachment,
                            };
                        }
                    }
                });
                let rest = ui.available_width() - theme::CONTROL_H - ui.spacing().item_spacing.x;
                match condition {
                    Condition::From(text) | Condition::To(text) | Condition::Subject(text) => {
                        ui.add(crate::sized_field(text, rest).hint_text("Text, in any case"));
                    }
                    Condition::Attachment => {
                        ui.add_space(rest);
                    }
                }
                if crate::icons::button(ui, t, crate::icons::Icon::X, "Remove condition", true, false).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            filter.conditions.remove(i);
        }
        if add(ui, t, "Add condition") {
            filter.conditions.push(Condition::From(String::new()));
        }

        section(ui, "Then");
        if filter.actions.is_empty() {
            ui.label(RichText::new("Nothing yet: add what to do with the mail.").size(13.0).color(t.text_dim));
        }
        let mut remove = None;
        for (i, action) in filter.actions.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let kinds = [("Move to", FilterAction::Move(String::new())), ("Mark as read", FilterAction::Read), ("Star", FilterAction::Star), ("Label", FilterAction::Label(1)), ("Move to Trash", FilterAction::Trash)];
                let shown = kinds.iter().find(|(_, k)| std::mem::discriminant(k) == std::mem::discriminant(action)).map_or("", |(l, _)| l);
                egui::ComboBox::from_id_salt(("filter_action", i)).width(KIND).selected_text(shown).show_ui(ui, |ui| {
                    for (label, kind) in kinds {
                        if ui.selectable_label(std::mem::discriminant(&kind) == std::mem::discriminant(action), label).clicked() {
                            *action = kind;
                        }
                    }
                });
                let rest = ui.available_width() - theme::CONTROL_H - ui.spacing().item_spacing.x;
                match action {
                    FilterAction::Move(folder) => {
                        let shown = folders.iter().find(|(n, _)| n == folder).map_or("Choose a folder", |(_, t)| t.as_str());
                        egui::ComboBox::from_id_salt(("filter_folder", i)).width(rest - 2.0 * ui.spacing().button_padding.x).selected_text(shown).height(320.0).show_ui(ui, |ui| {
                            for (name, title) in folders {
                                ui.selectable_value(folder, name.clone(), title);
                            }
                        });
                    }
                    FilterAction::Label(n) => {
                        egui::ComboBox::from_id_salt(("filter_label", i)).width(rest - 2.0 * ui.spacing().button_padding.x).selected_text(theme::label(&format!("$label{n}")).0).show_ui(ui, |ui| {
                            for k in 1..=5u8 {
                                ui.selectable_value(n, k, theme::label(&format!("$label{k}")).0);
                            }
                        });
                    }
                    _ => {
                        ui.add_space(rest);
                    }
                }
                if crate::icons::button(ui, t, crate::icons::Icon::X, "Remove action", true, false).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            filter.actions.remove(i);
        }
        if add(ui, t, "Add action") {
            filter.actions.push(if filter.actions.iter().any(|a| matches!(a, FilterAction::Move(_) | FilterAction::Trash)) { FilterAction::Read } else { FilterAction::Move(String::new()) });
        }
        ui.add_space(12.0);
        crate::views::switch_row(ui, t, &mut filter.stop, "Stop here", "The filters after this one are not tried on mail it matched. One that moves mail always stops.");
        ui.add_space(12.0);
        let why = problem(filter);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            if crate::views::primary_button(ui, t, "Save filter", why.is_none()).on_disabled_hover_text(why.unwrap_or_default()).clicked() {
                done = Some(true);
            }
            if ui.button("Cancel").clicked() {
                done = Some(false);
            }
        });
    });
    if modal.should_close() { Some(false) } else { done }
}

/// The editor's width: room for a kind, its text and the remove button on one row.
const WIDTH: f32 = 560.0;
/// The width of the drop-down that picks a condition's or an action's kind.
const KIND: f32 = 170.0;

/// A part of the editor: "When", "Then".
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(20.0);
    ui.label(RichText::new(title).size(theme::SUBTITLE).family(theme::bold()));
    ui.add_space(2.0);
}

/// "+ Add condition": a quiet link-like button, so the dialog's one filled button stays Save.
fn add(ui: &mut egui::Ui, t: &Tokens, label: &str) -> bool {
    let button = egui::Button::image_and_text(crate::icons::Icon::Plus.image(16.0, t.accent), RichText::new(label).color(t.accent)).frame(false);
    ui.add(button).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Why `filter` cannot be saved yet, if it cannot.
fn problem(filter: &Filter) -> Option<&'static str> {
    if filter.conditions.is_empty() {
        Some("Add a condition")
    } else if filter.conditions.iter().any(|c| !matches!(c, Condition::Attachment) && condition_text(c).trim().is_empty()) {
        Some("Fill in every condition's text")
    } else if filter.actions.is_empty() {
        Some("Add an action")
    } else if filter.actions.iter().any(|a| matches!(a, FilterAction::Move(f) if f.is_empty())) {
        Some("Choose the folder to move to")
    } else {
        None
    }
}

fn condition_text(condition: &Condition) -> &str {
    match condition {
        Condition::From(t) | Condition::To(t) | Condition::Subject(t) => t,
        Condition::Attachment => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(from: &str, address: &str, subject: &str, attach: bool) -> MessageSummary {
        MessageSummary { from: from.into(), address: address.into(), subject: subject.into(), has_attachments: attach, ..MessageSummary::default() }
    }

    fn filter(all: bool, conditions: Vec<Condition>, actions: Vec<FilterAction>) -> Filter {
        Filter { account: "ada@example.org".into(), name: "Receipts".into(), enabled: true, all, conditions, actions, stop: false }
    }

    #[test]
    fn a_filter_holds_on_any_or_every_condition_and_never_when_empty() {
        let m = message("Shop", "orders@shop.example.com", "Your Invoice 42", true);
        let any = filter(false, vec![Condition::From("nobody".into()), Condition::Subject("invoice".into())], vec![]);
        assert!(matches(&any, &m, ""));
        let every = Filter { all: true, ..any.clone() };
        assert!(!matches(&every, &m, ""), "the sender does not match");
        assert!(matches(&filter(true, vec![Condition::From("SHOP.example".into()), Condition::Attachment], vec![]), &m, ""));
        assert!(matches(&filter(true, vec![Condition::To("team@".into())], vec![]), &m, "team@example.org bob@example.com"));
        assert!(!matches(&filter(true, vec![], vec![]), &m, ""), "no conditions, no mail");
        assert!(!matches(&filter(true, vec![Condition::Subject("  ".into())], vec![]), &m, ""), "a blank text matches nothing");
    }

    #[test]
    fn flags_come_first_one_move_last_and_a_move_or_stop_ends_the_run() {
        let m = message("Shop", "orders@shop.example.com", "Invoice", false);
        let star = filter(false, vec![Condition::Subject("invoice".into())], vec![FilterAction::Star, FilterAction::Move("Receipts".into())]);
        let read = filter(false, vec![Condition::From("shop".into())], vec![FilterAction::Read, FilterAction::Trash]);
        let got = actions(&[star.clone(), read.clone()], &m, "");
        assert!(matches!(got.as_slice(), [Action::Star(true), Action::Move(f)] if f == "Receipts"), "the second filter is not tried");
        let stop = Filter { stop: true, actions: vec![FilterAction::Label(2)], ..star.clone() };
        assert!(matches!(actions(&[stop, read.clone()], &m, "").as_slice(), [Action::Label(2, true)]));
        let off = Filter { enabled: false, ..star };
        assert!(matches!(actions(&[off, read], &m, "").as_slice(), [Action::Unread(false), Action::Trash]));
    }
}
