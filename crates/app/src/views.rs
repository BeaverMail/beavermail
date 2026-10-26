//! Painted building blocks of the main window: folder rows, message rows, avatars, search box,
//! empty states. All colours come from [`Tokens`]; all sizes are named here or in `theme`.

use std::sync::Arc;

use beavermail_core::{Attachment, MessageSummary, Role};
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, Rect, Response, Sense, Ui, pos2, vec2};
use egui::text::{LayoutJob, TextWrapping};

use crate::html::{self, Block, Kind, Run};
use crate::icons::{self, Icon};
use crate::theme::{self, CONTROL_H, CONTROL_RADIUS, Tokens};

/// Fixed row height, so the list can be virtualised with `show_rows`.
pub const MESSAGE_ROW_H: f32 = 84.0;
pub const FOLDER_ROW_H: f32 = 38.0;
const THREAD_ROW_H: f32 = 38.0;
const SWITCH: egui::Vec2 = vec2(40.0, 22.0);
pub const PALETTE_ROW_H: f32 = 36.0;
pub const AVATAR: f32 = 40.0;
pub const PERSON_ROW_H: f32 = 60.0;
const PERSON_AVATAR: f32 = 34.0;

fn tint(t: &Tokens) -> Color32 {
    t.accent.gamma_multiply(0.16)
}

/// Single-line text that ends in an ellipsis instead of overflowing `width`.
pub fn truncated(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
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

/// The levels of a folder's name, split at the server's separator; where it is not known (a search
/// result, an archive from before it was kept), at "/" and at ".", the two servers use.
fn levels(name: &str, delimiter: Option<char>) -> Vec<&str> {
    match delimiter {
        Some(d) => name.split(d).collect(),
        None => name.split(['/', '.']).collect(),
    }
}

/// The folder name as shown: INBOX in capitals is noise, and nested names show their last part.
pub fn folder_title(name: &str, role: Role, delimiter: Option<char>) -> String {
    if role == Role::Inbox {
        return "Inbox".to_owned();
    }
    beavermail_imap::folder_name::decode(levels(name, delimiter).last().unwrap_or(&name))
}

/// A folder's whole place as people read it, for lists that name folders of every level: "Inbox ›
/// Clients" for "INBOX/Clients" or "INBOX.Clients".
pub fn folder_path(name: &str, role: Role, delimiter: Option<char>) -> String {
    if role == Role::Inbox {
        return "Inbox".to_owned();
    }
    let parts: Vec<String> = levels(name, delimiter)
        .into_iter()
        .enumerate()
        .map(|(i, part)| if i == 0 && part.eq_ignore_ascii_case("INBOX") { "Inbox".to_owned() } else { beavermail_imap::folder_name::decode(part) })
        .collect();
    parts.join(" › ")
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

/// The folders in sidebar order, as (position in `mailboxes`, depth): those with a role first, always at
/// the top, then the rest by name; a folder without a role goes under the one its name is inside, as
/// "INBOX.Receipts" under the Inbox, by each folder's own separator.
pub fn folder_tree(mailboxes: &[beavermail_core::Mailbox]) -> Vec<(usize, usize)> {
    let parent: Vec<Option<usize>> = mailboxes
        .iter()
        .map(|m| (m.role == Role::Other).then(|| (0..mailboxes.len()).filter(|&j| is_inside(&m.name, &mailboxes[j].name, m.delimiter)).max_by_key(|&j| mailboxes[j].name.len())).flatten())
        .collect();
    let sorted = |mut list: Vec<usize>| {
        list.sort_by_key(|&i| (folder_rank(mailboxes[i].role), beavermail_imap::folder_name::decode(&mailboxes[i].name).to_lowercase()));
        list
    };
    let mut stack: Vec<(usize, usize)> = sorted((0..mailboxes.len()).filter(|&i| parent[i].is_none()).collect()).into_iter().rev().map(|i| (i, 0)).collect();
    let mut out = Vec::with_capacity(mailboxes.len());
    while let Some((i, depth)) = stack.pop() {
        out.push((i, depth));
        stack.extend(sorted((0..mailboxes.len()).filter(|&j| parent[j] == Some(i)).collect()).into_iter().rev().map(|j| (j, depth + 1)));
    }
    out
}

/// The arrow of what folds in a sidebar (an account, a server, a folder with others inside), all one size.
pub fn fold_arrow(ui: &mut Ui, openness: f32, response: &Response) {
    const SIDE: f32 = 12.0;
    let drawn = response.clone().with_new_rect(Rect::from_center_size(response.rect.center(), vec2(SIDE, SIDE)));
    egui::collapsing_header::paint_default_icon(ui, openness, &drawn);
}

/// Whether folder `child` is inside folder `parent`, at any depth ("INBOX.Receipts.Old" in "INBOX"), by
/// the server's separator (see [`levels`] without it).
pub fn is_inside(child: &str, parent: &str, delimiter: Option<char>) -> bool {
    let Some(rest) = child.strip_prefix(parent) else { return false };
    let mut rest = rest.chars();
    let next = rest.next();
    rest.next().is_some() && next.is_some_and(|c| delimiter.map_or(c == '/' || c == '.', |d| c == d))
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
    nav_row(ui, t, title, Some((role_icon(role), theme::role_color(role))), unread, selected)
}

/// Under an Inbox: the mail held apart as suspicious, with how many messages in the pill.
pub fn suspicious_row(ui: &mut Ui, t: &Tokens, held: u32, selected: bool) -> Response {
    nav_row(ui, t, "Suspicious", Some((Icon::Spam, t.danger)), held, selected)
}

/// A page in the settings sidebar: its icon, or with none the initials of `title` (an account).
pub fn settings_row(ui: &mut Ui, t: &Tokens, title: &str, icon: Option<Icon>, selected: bool) -> Response {
    let response = nav_row(ui, t, title, icon.map(|i| (i, if selected { t.accent } else { t.text_dim })), 0, selected);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, title));
    response
}

fn nav_row(ui: &mut Ui, t: &Tokens, title: &str, icon: Option<(Icon, Color32)>, unread: u32, selected: bool) -> Response {
    // A sidebar made denser (see [`theme::dense`]) lowers it.
    let height = FOLDER_ROW_H.min(ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    row_background(ui, rect, &response, selected, t);
    let lead = pos2(rect.left() + 22.0, rect.center().y);
    match icon {
        Some((icon, color)) => icon.image(icons::SIZE, color).paint_at(ui, Rect::from_center_size(lead, vec2(icons::SIZE, icons::SIZE))),
        None => avatar(ui, t, title, lead, 24.0),
    }
    if response.has_focus() {
        ui.painter().rect_stroke(rect, CornerRadius::same(CONTROL_RADIUS), egui::Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
    }
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

/// A line of the command palette: what it does on the left, its keys on the right; `enabled` false
/// dims it (the caller says why on hover).
pub fn palette_row(ui: &mut Ui, t: &Tokens, label: &str, keys: &str, chosen: bool, enabled: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), PALETTE_ROW_H), Sense::click());
    row_background(ui, rect, &response, chosen, t);
    let color = if enabled { t.text } else { t.text_dim.gamma_multiply(0.7) };
    let mut right = rect.right() - 12.0;
    if !keys.is_empty() {
        let galley = ui.painter().layout_no_wrap(keys.to_owned(), FontId::proportional(12.5), t.text_dim);
        let cap = Rect::from_min_size(pos2(right - galley.size().x - 12.0, rect.center().y - 11.0), vec2(galley.size().x + 12.0, 22.0));
        ui.painter().rect_stroke(cap, CornerRadius::same(5), egui::Stroke::new(1.0, t.border_strong), egui::StrokeKind::Inside);
        ui.painter().galley(cap.center() - galley.size() / 2.0, galley, t.text_dim);
        right = cap.left() - 12.0;
    }
    let text = truncated(ui, label, FontId::proportional(14.5), color, right - rect.left() - 12.0);
    ui.painter().galley(pos2(rect.left() + 12.0, rect.center().y - text.size().y / 2.0), text, color);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, chosen, label));
    if enabled { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response }
}

/// A month to pick a day from: the chosen `day` filled, today ringed, days before `earliest` (when
/// given) dimmed and out of reach; ‹ › move `month` (its first day). Drawn in place, so it works inside a menu.
pub fn calendar(ui: &mut Ui, t: &Tokens, day: &mut chrono::NaiveDate, month: &mut chrono::NaiveDate, today: chrono::NaiveDate, earliest: Option<chrono::NaiveDate>) {
    use chrono::{Datelike, Months};
    const CELL: egui::Vec2 = vec2(34.0, 30.0);
    let width = CELL.x * 7.0;
    ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
    // Rows of days, not controls: the theme's 40-point control height would space the weeks apart.
    ui.spacing_mut().interact_size.y = 0.0;
    ui.allocate_ui_with_layout(vec2(width, CELL.y), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        let first = earliest.map(|e| e.with_day(1).unwrap_or(e));
        let arrow = |ui: &mut Ui, text: &str, enabled: bool| ui.add_enabled(enabled, egui::Button::new(egui::RichText::new(text).size(18.0)).frame(false).min_size(CELL)).clicked();
        if arrow(ui, "‹", first.is_none_or(|first| *month > first)) {
            *month = month.checked_sub_months(Months::new(1)).unwrap_or(*month);
        }
        let title = month.format("%B %Y").to_string();
        ui.allocate_ui_with_layout(vec2(width - CELL.x * 2.0, CELL.y), egui::Layout::centered_and_justified(egui::Direction::LeftToRight), |ui| {
            ui.label(egui::RichText::new(title).family(theme::bold()));
        });
        if arrow(ui, "›", true) {
            *month = month.checked_add_months(Months::new(1)).unwrap_or(*month);
        }
    });
    let cell_text = |ui: &Ui, rect: Rect, text: &str, color: Color32, size: f32| {
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, FontId::proportional(size), color);
    };
    ui.horizontal(|ui| {
        for name in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] {
            let (rect, _) = ui.allocate_exact_size(CELL, Sense::hover());
            cell_text(ui, rect, name, t.text_dim, 12.0);
        }
    });
    let lead = month.weekday().num_days_from_monday() as usize;
    let days = month.checked_add_months(Months::new(1)).map_or(30, |next| (next - *month).num_days()) as usize;
    for week in 0..(lead + days).div_ceil(7) {
        ui.horizontal(|ui| {
            for slot in 0..7 {
                let n = week * 7 + slot;
                if n < lead || n >= lead + days {
                    ui.allocate_exact_size(CELL, Sense::hover());
                    continue;
                }
                let date = *month + chrono::Duration::days((n - lead) as i64);
                let open = earliest.is_none_or(|e| date >= e);
                let (rect, response) = ui.allocate_exact_size(CELL, if open { Sense::click() } else { Sense::hover() });
                let spot = Rect::from_center_size(rect.center(), vec2(28.0, 26.0));
                let chosen = date == *day;
                if chosen {
                    ui.painter().rect_filled(spot, CornerRadius::same(CONTROL_RADIUS), t.accent);
                } else if open && response.hovered() {
                    ui.painter().rect_filled(spot, CornerRadius::same(CONTROL_RADIUS), t.hover);
                }
                if date == today && !chosen {
                    ui.painter().rect_stroke(spot, CornerRadius::same(CONTROL_RADIUS), egui::Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
                }
                let color = if chosen { t.on_accent } else if open { t.text } else { t.text_dim.gamma_multiply(0.5) };
                cell_text(ui, rect, &date.day().to_string(), color, 14.0);
                if response.clicked() {
                    *day = date;
                }
                if open {
                    response.on_hover_cursor(egui::CursorIcon::PointingHand).widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, chosen, date.format("%A %-d %B").to_string()));
                }
            }
        });
    }
}

/// An on/off setting: its name and a line saying what it does on the left, a switch on the right.
/// The whole row flips it; the response is `changed` when it did.
pub fn switch_row(ui: &mut Ui, t: &Tokens, on: &mut bool, label: &str, hint: &str) -> Response {
    let width = ui.available_width();
    let text_w = (width - SWITCH.x - 24.0).max(80.0);
    let title = ui.painter().layout(label.to_owned(), FontId::proportional(15.0), t.text, text_w);
    let about = (!hint.is_empty()).then(|| ui.painter().layout(hint.to_owned(), FontId::proportional(13.0), t.text_dim, text_w));
    let text_h = title.size().y + about.as_ref().map_or(0.0, |a| a.size().y + 3.0);
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, text_h.max(SWITCH.y) + 12.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let top = rect.top() + 6.0;
    ui.painter().galley(pos2(rect.left(), top), title, t.text);
    if let Some(about) = about {
        ui.painter().galley(pos2(rect.left(), rect.bottom() - 6.0 - about.size().y), about, t.text_dim);
    }
    // The switch lines up with the name, where the eye reads what it turns on.
    let track = Rect::from_min_size(pos2(rect.right() - SWITCH.x, top + 10.0 - SWITCH.y / 2.0), SWITCH);
    let k = ui.ctx().animate_bool_responsive(response.id, *on);
    let off = if t.dark { t.border_strong } else { t.text_dim };
    let fill = if *on { t.accent } else { off };
    let fill = if response.hovered() { fill.gamma_multiply(0.85) } else { fill };
    ui.painter().rect_filled(track, CornerRadius::same((SWITCH.y / 2.0) as u8), fill);
    let knob = if *on || !t.dark { t.on_accent } else { t.text };
    let r = SWITCH.y / 2.0 - 3.0;
    ui.painter().circle_filled(pos2(track.left() + 3.0 + r + k * (SWITCH.x - 6.0 - 2.0 * r), track.center().y), r, knob);
    if response.has_focus() {
        ui.painter().rect_stroke(track.expand(3.0), CornerRadius::same((SWITCH.y / 2.0 + 3.0) as u8), egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Outside);
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One message of the open conversation in the reader: who, a line of it, and when; `current` is the one shown.
pub fn thread_row(ui: &mut Ui, t: &Tokens, message: &MessageSummary, current: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), THREAD_ROW_H), Sense::click());
    row_background(ui, rect, &response, current, t);
    avatar(ui, t, &message.from, pos2(rect.left() + 20.0, rect.center().y), 24.0);
    let date = ui.painter().layout_no_wrap(message.received.clone(), FontId::proportional(13.0), t.text_dim);
    let date_w = date.size().x;
    ui.painter().galley(pos2(rect.right() - 10.0 - date_w, rect.center().y - date.size().y / 2.0), date, t.text_dim);
    let left = rect.left() + 42.0;
    let room = rect.right() - 20.0 - date_w - left;
    let color = if message.unread || current { t.text } else { t.text_dim };
    let name = truncated(ui, &message.from, FontId::proportional(14.0), color, room * 0.4);
    let name_w = name.size().x;
    ui.painter().galley(pos2(left, rect.center().y - name.size().y / 2.0), name, color);
    let line = truncated(ui, &message.preview, FontId::proportional(13.0), t.text_dim, room - name_w - 10.0);
    ui.painter().galley(pos2(left + name_w + 10.0, rect.center().y - line.size().y / 2.0), line, t.text_dim);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, current, &message.from));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A person in the Contacts list: avatar, name and a dim line under it (an address, where they are).
/// `photo`: theirs, drawn in the avatar's place.
pub fn person_row(ui: &mut Ui, t: &Tokens, name: &str, line: &str, photo: Option<&egui::TextureHandle>, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), PERSON_ROW_H), Sense::click());
    let card = rect.shrink2(vec2(8.0, 2.0));
    row_background(ui, card, &response, selected, t);
    let center = pos2(card.left() + 12.0 + PERSON_AVATAR / 2.0, card.center().y);
    match photo {
        Some(photo) => egui::Image::from_texture(photo).fit_to_exact_size(vec2(PERSON_AVATAR, PERSON_AVATAR)).corner_radius(PERSON_AVATAR / 2.0).paint_at(ui, Rect::from_center_size(center, vec2(PERSON_AVATAR, PERSON_AVATAR))),
        None => avatar(ui, t, name, center, PERSON_AVATAR),
    }
    let left = card.left() + 12.0 + PERSON_AVATAR + 12.0;
    let width = card.right() - 12.0 - left;
    let title = truncated(ui, name, FontId::proportional(15.0), t.text, width);
    if line.is_empty() {
        ui.painter().galley(pos2(left, card.center().y - title.size().y / 2.0), title, t.text);
    } else {
        let under = truncated(ui, line, FontId::proportional(13.0), t.text_dim, width);
        let top = card.center().y - (title.size().y + 2.0 + under.size().y) / 2.0;
        let title_h = title.size().y;
        ui.painter().galley(pos2(left, top), title, t.text);
        ui.painter().galley(pos2(left, top + title_h + 2.0), under, t.text_dim);
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, name));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A message in the list: avatar, sender with unread dot, date, subject and a one-line preview.
/// `warning`, the spam check that failed, takes the preview's place (the preview is the sender's
/// text), so spam is judged without opening it. `count`: messages in its conversation, shown from two.
/// `mark`: the colour of its account, in All inboxes.
#[allow(clippy::too_many_arguments)]
pub fn message_row(ui: &mut Ui, t: &Tokens, message: &MessageSummary, selected: bool, folder: Option<&str>, warning: Option<&str>, count: usize, mark: Option<Color32>) -> Response {
    // Clicked, never focused: the arrow keys move the list's own selection (`handle_keys`), and a focused
    // row would have egui move its focus with them too, scrolling the list elsewhere. Dragged onto a folder,
    // it moves there.
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), MESSAGE_ROW_H), Sense::click_and_drag());
    let card = rect.shrink2(vec2(8.0, 2.0));
    row_background(ui, card, &response, selected, t);
    if let Some(color) = mark {
        ui.painter().rect_filled(Rect::from_min_size(pos2(card.left() + 3.0, card.top() + 14.0), vec2(3.0, card.height() - 28.0)), CornerRadius::same(2), color);
    }
    avatar(ui, t, &message.from, pos2(card.left() + 14.0 + AVATAR / 2.0, card.top() + 14.0 + AVATAR / 2.0), AVATAR);
    let left = card.left() + 14.0 + AVATAR + 12.0;
    let width = card.right() - 14.0 - left;

    let date = ui.painter().layout_no_wrap(message.received.clone(), FontId::proportional(13.0), t.text_dim);
    let date_w = date.size().x;
    ui.painter().galley(pos2(card.right() - 14.0 - date_w, card.top() + 13.0), date, t.text_dim);

    // The paperclip sits just left of the date, where the eye already is.
    let clip = if message.has_attachments { 20.0 } else { 0.0 };
    if message.has_attachments {
        Icon::Paperclip.image(15.0, t.text_dim).paint_at(ui, Rect::from_center_size(pos2(card.right() - 14.0 - date_w - 17.0, card.top() + 21.0), vec2(15.0, 15.0)));
    }
    let mut clip = clip;
    // Pinned here, and starred (a flag from any client): beside the paperclip.
    for (on, icon, color) in [(message.pinned, Icon::Pin, t.accent), (message.starred, Icon::Star, theme::role_color(Role::Starred))] {
        if on {
            icon.image(15.0, color).paint_at(ui, Rect::from_center_size(pos2(card.right() - 14.0 - date_w - clip - 17.0, card.top() + 21.0), vec2(15.0, 15.0)));
            clip += 20.0;
        }
    }
    if count > 1 {
        let text = ui.painter().layout_no_wrap(count.to_string(), FontId::proportional(12.5), t.text_dim);
        let pill = Rect::from_center_size(pos2(0.0, card.top() + 21.0), vec2((text.size().x + 14.0).max(24.0), 20.0));
        let pill = pill.translate(vec2(card.right() - 14.0 - date_w - clip - 8.0 - pill.width() / 2.0, 0.0));
        ui.painter().rect_filled(pill, CornerRadius::same(10), if selected { t.accent.gamma_multiply(0.22) } else { t.hover });
        ui.painter().galley(pill.center() - text.size() / 2.0, text, t.text_dim);
        clip += pill.width() + 8.0;
    }
    let primary = if message.unread { t.text } else { t.text_dim };
    let dot = if message.unread { 16.0 } else { 0.0 };
    // The name is free text anyone can set, so the domain it really comes from is shown beside it.
    let domain = crate::sender::domain(&message.address);
    let flagged = crate::sender::warning(&message.from, &message.address).is_some();
    let room = width - date_w - 12.0 - dot - clip;
    let name_room = if domain.is_empty() { room } else { room * 0.6 };
    // Unread: the sender and subject in bold, beside the dot.
    let weight = if message.unread { theme::bold() } else { egui::FontFamily::Proportional };
    let sender = truncated(ui, &message.from, FontId::new(15.5, weight.clone()), primary, name_room);
    let sender_w = sender.size().x;
    ui.painter().galley(pos2(left, card.top() + 11.0), sender, primary);
    let mut x = left + sender_w + 10.0;
    if message.unread {
        ui.painter().circle_filled(pos2(x, card.top() + 21.0), 4.0, t.accent);
        x += 12.0;
    }
    if !domain.is_empty() && message.from != message.address {
        let color = if flagged { t.danger } else { t.text_dim };
        if flagged {
            Icon::Spam.image(14.0, color).paint_at(ui, Rect::from_center_size(pos2(x + 7.0, card.top() + 21.0), vec2(14.0, 14.0)));
            x += 18.0;
        }
        let shown = truncated(ui, &domain, FontId::proportional(13.0), color, (left + room - x).max(0.0));
        ui.painter().galley(pos2(x, card.top() + 13.0), shown, color);
    }
    // The subject line: what the sender marked as high or low priority before it, labels after it.
    let mut subject_x = left;
    match message.priority {
        1 => Icon::CircleAlert.image(14.0, t.danger).paint_at(ui, Rect::from_center_size(pos2(left + 7.0, card.top() + 42.0), vec2(14.0, 14.0))),
        -1 => Icon::ArrowDown.image(14.0, t.text_dim).paint_at(ui, Rect::from_center_size(pos2(left + 7.0, card.top() + 42.0), vec2(14.0, 14.0))),
        _ => {}
    }
    if message.priority != 0 {
        subject_x += 20.0;
    }
    let mut chips_right = card.right() - 14.0;
    for keyword in message.labels.iter().rev() {
        let (name, color) = theme::label(keyword);
        let text = ui.painter().layout_no_wrap(name, FontId::proportional(12.0), t.text_dim);
        let pill = Rect::from_min_size(pos2(chips_right - text.size().x - 22.0, card.top() + 33.0), vec2(text.size().x + 22.0, 18.0));
        if pill.left() < subject_x + 80.0 {
            break; // the subject keeps room; labels that do not fit stay off the row
        }
        ui.painter().rect_filled(pill, CornerRadius::same(9), t.hover);
        ui.painter().circle_filled(pos2(pill.left() + 9.0, pill.center().y), 3.5, color);
        ui.painter().galley(pos2(pill.left() + 16.0, pill.center().y - text.size().y / 2.0), text, t.text_dim);
        chips_right = pill.left() - 6.0;
    }
    let subject = truncated(ui, &message.subject, FontId::new(14.5, weight), primary, chips_right - subject_x - 6.0);
    ui.painter().galley(pos2(subject_x, card.top() + 33.0), subject, primary);
    // In search results: which folder the message is in, so identical subjects can be told apart.
    let label = folder.map(|f| ui.painter().layout_no_wrap(folder_title(f, Role::Other, None), FontId::proportional(12.5), t.accent));
    let label_w = label.as_ref().map_or(0.0, |l| l.size().x + 12.0);
    match warning {
        Some(text) => {
            Icon::Spam.image(14.0, t.danger).paint_at(ui, Rect::from_center_size(pos2(left + 7.0, card.top() + 64.0), vec2(14.0, 14.0)));
            let line = truncated(ui, text, FontId::proportional(13.5), t.danger, width - label_w - 20.0);
            ui.painter().galley(pos2(left + 20.0, card.top() + 56.0), line, t.danger);
        }
        None => {
            let preview = truncated(ui, &message.preview.chars().take(200).collect::<String>(), FontId::proportional(13.5), t.text_dim, width - label_w);
            ui.painter().galley(pos2(left, card.top() + 56.0), preview, t.text_dim);
        }
    }
    if let Some(label) = label {
        ui.painter().galley(pos2(card.right() - 14.0 - label.size().x, card.top() + 57.0), label, t.accent);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Above a message the checks have an opinion on: why, and the one correction that makes sense. True
/// when the button is pressed.
/// Which button of [`mark_bar`] was pressed.
pub enum Mark {
    /// The bar's own correction (Spam, Not spam).
    Correct,
    Trust,
}

/// `trust`: the tooltip of a Trust button beside the correction, for a new sender.
pub fn mark_bar(ui: &mut Ui, t: &Tokens, text: &str, held: bool, button: &str, tip: &str, trust: Option<&str>) -> Option<Mark> {
    let mut pressed = None;
    egui::Frame::new().fill(t.input).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(CornerRadius::same(CONTROL_RADIUS)).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.add(Icon::Spam.image(icons::SIZE, if held { t.danger } else { t.text_dim }));
            ui.label(egui::RichText::new(text).size(13.5).color(t.text));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(trust_tip) = trust
                && ui.button("Trust").on_hover_text(trust_tip).clicked()
            {
                pressed = Some(Mark::Trust);
            }
            if ui.button(button).on_hover_text(tip).clicked() {
                pressed = Some(Mark::Correct);
            }
        });
    });
    pressed
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

/// The reader while several messages are picked in the list: how many, and what can be done to all.
/// Move to folder… sets `moving`, as it asks for the folder first.
pub fn picked_panel(ui: &mut Ui, t: &Tokens, count: usize, any_unread: bool, role: Role, moving: &mut bool) -> Option<crate::Action> {
    use crate::Action;
    let mut chosen = None;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.3).max(24.0));
        ui.heading(format!("{} messages selected", thousands(count as u32)));
        ui.label(egui::RichText::new("Ctrl+click adds or removes one, Shift+click picks a range, Esc clears.").color(t.text_dim));
        ui.add_space(16.0);
        let mut commands = vec![
            (Icon::Archive, "Archive", "E", Action::Archive, role != Role::Archive),
            if any_unread { (Icon::MailOpen, "Mark as read", "U", Action::Unread(false), true) } else { (Icon::Mail, "Mark as unread", "U", Action::Unread(true), true) },
            (Icon::Trash, if role == Role::Trash { "Delete permanently" } else { "Move to Trash" }, "Delete", Action::Trash, true),
        ];
        match role {
            Role::Inbox => commands.push((Icon::Spam, "Spam", "!", Action::Spam, true)),
            Role::Junk => commands.push((Icon::Spam, "Not spam", "", Action::NotSpam, true)),
            _ => {}
        }
        for (icon, label, key, action, enabled) in commands {
            let button = egui::Button::image_and_text(icon.image(icons::SIZE, t.text_dim), label).min_size(vec2(220.0, CONTROL_H));
            let response = ui.add_enabled(enabled, button);
            let response = if key.is_empty() { response } else { response.on_hover_text(format!("{label} ({key})")) };
            if response.clicked() {
                chosen = Some(action);
            }
        }
        let button = egui::Button::image_and_text(Icon::Folder.image(icons::SIZE, t.text_dim), "Move to folder…").min_size(vec2(220.0, CONTROL_H));
        *moving |= ui.add(button).on_hover_text("Move to folder… (V)").clicked();
    });
    chosen
}

/// What the user chose in the remote-content bar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    /// Load it for this message.
    Once,
    /// Load it for this message and every later one from the sender.
    Always,
    /// Stop loading it (and forget the sender's permission).
    Block,
}

/// How a note under the sender reads: all is well, for information, or a warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Quiet,
    Warning,
}

/// A line under the sender: an icon and small text, as tall as the text (not a 40-point control);
/// `tip` on hover when not empty.
pub fn note(ui: &mut Ui, icon: Icon, color: Color32, text: &str, tip: &str) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 0.0;
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal_wrapped(|ui| {
            ui.add(icon.image(crate::icons::SIZE, color));
            let label = ui.label(egui::RichText::new(text).size(13.0).color(color));
            if !tip.is_empty() {
                label.on_hover_text(tip);
            }
        });
    });
}

/// What encryption and signature say, as notes: "Encrypted  ·  Signed by ada@…" in one line when both
/// hold (the full wording on hover); a problem, or a key not known here, on a line of its own.
pub fn security_notes(encryption: Option<&beavermail_core::Encryption>, signature: Option<&beavermail_core::Signature>) -> Vec<(String, String, Tone)> {
    let (mut short, mut tips, mut notes) = (Vec::new(), Vec::new(), Vec::new());
    match encryption {
        Some(e @ beavermail_core::Encryption::Opened(_)) => {
            short.push("Encrypted".to_owned());
            tips.push(encryption_line(e).0);
        }
        Some(e) => {
            let (line, danger) = encryption_line(e);
            notes.push((line, String::new(), if danger { Tone::Warning } else { Tone::Quiet }));
        }
        None => {}
    }
    match signature {
        Some(s @ beavermail_core::Signature { check: beavermail_core::SignatureCheck::Valid { signer, .. }, .. }) => {
            short.push(format!("Signed by {signer}"));
            tips.push(signature_line(s).0);
        }
        Some(s) => {
            let (line, danger) = signature_line(s);
            notes.push((line, String::new(), if danger { Tone::Warning } else { Tone::Quiet }));
        }
        None => {}
    }
    if !short.is_empty() {
        notes.insert(0, (short.join("  ·  "), tips.join("\n"), Tone::Good));
    }
    notes
}

/// What became of an encrypted message, in one line, and whether it is a warning.
pub fn encryption_line(encryption: &beavermail_core::Encryption) -> (String, bool) {
    use beavermail_core::Encryption as E;
    match encryption {
        E::Opened(_) => ("Encrypted: only you and the other recipients can read it".to_owned(), false),
        E::NoKey(_) => ("Encrypted for a key not on this computer. Turn on encryption or import your key in Settings > Encryption".to_owned(), false),
        E::Failed(_, why) => (format!("Encrypted, but it could not be opened: {why}"), true),
    }
}

/// What a message's signature says, in one line, and whether it is a warning.
pub fn signature_line(signature: &beavermail_core::Signature) -> (String, bool) {
    use beavermail_core::{SignatureCheck as C, SignedWith};
    let how = match signature.kind {
        SignedWith::Smime => "S/MIME",
        SignedWith::OpenPgp => "OpenPGP",
    };
    match &signature.check {
        C::Valid { signer, by } if signature.kind == SignedWith::Smime => (format!("Signed by {signer}  ·  certificate from {by} ({how})"), false),
        C::Valid { signer, by } => (format!("Signed by {signer}  ·  {by} ({how})"), false),
        C::NotSender { signer } => (format!("Signed by someone else than the sender: {signer}"), true),
        C::KeyChanged { signer } => (format!("Signed with a different key than {signer} used before: it may not be them"), true),
        C::UnknownKey { key } => (format!("Signed with a key not known here ({how}, {key})"), false),
        C::Changed => ("The message was changed after it was signed".to_owned(), true),
        C::Untrusted(why) => (format!("Signed, but the signature cannot be trusted: {why}"), true),
    }
}

/// "2 trackers blocked: list-manage.com, mailchimp.com" for the images that only report the message
/// was opened (never loaded, whatever is allowed): who would have been told, once each.
pub fn trackers_line(trackers: &[String]) -> Option<String> {
    let mut hosts: Vec<String> = trackers.iter().filter_map(|u| blitz_traits::net::Url::parse(u).ok()?.host_str().map(str::to_owned)).collect();
    hosts.sort();
    hosts.dedup();
    let n = trackers.len();
    (n > 0).then(|| format!("{n} tracker{} blocked: {}", if n == 1 { "" } else { "s" }, hosts.join(", ")))
}

/// The bar above a message shown as designed: says what is blocked or loaded and offers the choice.
pub fn remote_bar(ui: &mut Ui, t: &Tokens, loaded: bool, sender: &str) -> Option<Remote> {
    let mut choice = None;
    egui::Frame::new().fill(t.input).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(egui::CornerRadius::same(crate::theme::CONTROL_RADIUS)).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if loaded {
            ui.label(egui::RichText::new("Images and styles from the internet are loaded. The sender can see that you opened this message.").size(13.0).color(t.text_dim));
            if ui.button("Block again").clicked() {
                choice = Some(Remote::Block);
            }
        } else {
            ui.label(egui::RichText::new("Images and styles from the internet are blocked.").size(13.5).color(t.text));
            ui.label(egui::RichText::new("Loading them tells the sender you opened this message, and shows your network address.").size(13.0).color(t.text_dim));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if primary_button(ui, t, "Load for this message", true).clicked() {
                    choice = Some(Remote::Once);
                }
                if !sender.is_empty() && ui.button(format!("Always for {sender}")).clicked() {
                    choice = Some(Remote::Always);
                }
            });
        }
    });
    choice
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
    let button = egui::Button::image_and_text(icon.image(icons::SIZE, text), egui::RichText::new(label).color(text).family(theme::bold()))
        .fill(fill)
        .min_size(vec2(ui.available_width(), CONTROL_H));
    match why_disabled {
        Some(why) => ui.add(button.sense(Sense::hover())).on_hover_text(why),
        None => ui.add(button),
    }
}

/// The filled button with a leading icon and a fixed width.
pub fn primary_wide_fixed(ui: &mut Ui, t: &Tokens, icon: Icon, label: &str, enabled: bool, width: f32) -> Response {
    let button = egui::Button::image_and_text(icon.image(icons::SIZE, t.on_accent), egui::RichText::new(label).color(t.on_accent).family(theme::bold()))
        .fill(t.accent)
        .min_size(vec2(width, CONTROL_H));
    ui.add_enabled(enabled, button)
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 22.0,
        2 => 20.0,
        3 => 18.0,
        _ => 16.5,
    }
}

/// One line of runs that wraps like text. Returns the (cleaned) link the user clicked.
/// `mono`: a code block, all of it in a fixed-width font. `strong`: a heading, all of it bold.
fn runs_ui(ui: &mut Ui, t: &Tokens, runs: &[Run], size: f32, color: Color32, mono: bool, strong: bool) -> Option<String> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for run in runs {
            match run {
                Run::Text { text, bold, italic, code, strike } => {
                    let mut rich = egui::RichText::new(text).size(size).color(color);
                    if *bold || strong {
                        rich = rich.family(theme::bold());
                    }
                    if *italic {
                        rich = rich.italics();
                    }
                    if *strike {
                        rich = rich.strikethrough();
                    }
                    if mono {
                        rich = rich.monospace();
                    } else if *code {
                        // Inline code stands apart from the words around it, not only by its font.
                        rich = rich.monospace().size(size - 1.0).background_color(t.hover);
                    }
                    ui.label(rich);
                }
                Run::Link { text, url } => {
                    let link = LinkInfo::new(ui.ctx(), text, url);
                    let shown = if text.is_empty() { url } else { text };
                    let tint = if link.misleading { t.danger } else { t.accent };
                    let response = ui.add(egui::Link::new(egui::RichText::new(shown).size(size).color(tint))).on_hover_text(&link.tip);
                    if response.clicked() {
                        clicked = Some(link.clean.clone());
                    }
                    if let Some(open) = link_menu(&response, &link.clean) {
                        clicked = Some(open);
                    }
                }
            }
        }
    });
    clicked
}

/// Where the extra link parameters to remove (Settings > Reading) are kept for the frame.
fn link_params_id() -> egui::Id {
    egui::Id::new("link_params")
}

/// Tells the links drawn from now on which parameters the user also wants removed.
pub fn set_link_params(ctx: &egui::Context, params: &str) {
    ctx.data_mut(|d| d.insert_temp(link_params_id(), params.to_owned()));
}

/// A link as the reader shows it before it is followed.
pub struct LinkInfo {
    /// What it opens: straight to its target (out of a known redirect), without tracking parameters.
    pub clean: String,
    /// Its text names another site than the one it really goes to.
    pub misleading: bool,
    /// Where it really goes, said on hover.
    pub tip: String,
}

impl LinkInfo {
    pub fn new(ctx: &egui::Context, text: &str, url: &str) -> Self {
        let clean = html::clean_url(url, &ctx.data(|d| d.get_temp::<String>(link_params_id())).unwrap_or_default());
        // Judged by the real target: a redirect's own host says nothing about where the link leads.
        let misleading = html::looks_misleading(text, &clean);
        let mut tip = String::new();
        if misleading {
            tip += &format!("Careful: the text names another site.\nThis link goes to {}\n", html::host(&clean));
        }
        if clean.split(['?', '#']).next() != url.split(['?', '#']).next() {
            tip += &format!("Opens directly, without passing through {}\n", html::host(url));
        }
        tip += &clean;
        Self { clean, misleading, tip }
    }
}

/// Menus as egui draws them, with room around the text of each line (egui leaves 2 points).
pub fn menu_look(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding = vec2(10.0, 6.0);
}

/// A right-click menu on `response`, in [`menu_look`].
pub fn context_menu(response: &Response, add: impl FnOnce(&mut Ui)) {
    egui::Popup::context_menu(response).style(menu_look).show(add);
}

/// "Open link" and "Copy link" on a right click, both with the cleaned link; returns it when opened.
pub fn link_menu(response: &Response, clean: &str) -> Option<String> {
    let mut open = None;
    context_menu(response, |ui| {
        if ui.button("Open link").clicked() {
            open = Some(clean.to_owned());
        }
        if ui.button("Copy link").on_hover_text("Copied without tracking parameters").clicked() {
            ui.ctx().copy_text(clean.to_owned());
        }
    });
    open
}

/// A message body as blocks. Returns the link the user clicked, already cleaned of tracking parameters.
pub fn body(ui: &mut Ui, t: &Tokens, blocks: &[Block]) -> Option<String> {
    ui.spacing_mut().item_spacing.y = 2.0;
    // Lines of text, not controls: the theme's 40-point control height would make every row (a list
    // item, a line of a paragraph) that tall, with the text centred and a list's bullet left at the top.
    ui.spacing_mut().interact_size.y = 0.0;
    let mut clicked = None;
    for block in blocks {
        let indent = f32::from(block.depth) * 14.0;
        match block.kind {
            Kind::Rule => {
                ui.separator();
            }
            Kind::Heading(level) => {
                ui.add_space(6.0);
                clicked = runs_ui(ui, t, &block.runs, heading_size(level), t.text, false, true).or(clicked);
                ui.add_space(4.0);
            }
            Kind::Item => {
                ui.horizontal_top(|ui| {
                    ui.add_space(indent);
                    // A continued line keeps the marker's room, drawn invisibly, so it lines up with the item's text.
                    let marker_color = if block.continued { Color32::TRANSPARENT } else { t.text_dim };
                    ui.label(egui::RichText::new(block.marker.as_deref().unwrap_or("•")).size(16.0).color(marker_color));
                    clicked = runs_ui(ui, t, &block.runs, 16.0, t.text, false, false).or(clicked.take());
                });
                ui.add_space(2.0);
            }
            Kind::Quote => {
                let frame = egui::Frame::new().inner_margin(Margin { left: 12 + indent as i8, right: 0, top: 0, bottom: 0 });
                let shown = frame.show(ui, |ui| runs_ui(ui, t, &block.runs, 15.5, t.text_dim, false, false));
                let r = shown.response.rect;
                ui.painter().line_segment([pos2(r.left() + indent, r.top()), pos2(r.left() + indent, r.bottom())], egui::Stroke::new(2.0, t.border_strong));
                clicked = shown.inner.or(clicked);
            }
            Kind::Pre => {
                // A code block in its own box, as wide as the text column.
                let frame = egui::Frame::new().fill(t.hover).corner_radius(CornerRadius::same(CONTROL_RADIUS)).inner_margin(Margin::symmetric(12, 8));
                let shown = frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    runs_ui(ui, t, &block.runs, 14.5, t.text, true, false)
                });
                clicked = shown.inner.or(clicked);
                ui.add_space(6.0);
            }
            Kind::Table { header } => {
                clicked = table(ui, t, &block.rows, header).or(clicked);
                ui.add_space(10.0);
            }
            Kind::Para | Kind::Line => {
                if block.runs.is_empty() {
                    ui.add_space(8.0);
                } else {
                    clicked = runs_ui(ui, t, &block.runs, 16.0, t.text, false, false).or(clicked);
                    if block.kind == Kind::Para {
                        ui.add_space(8.0);
                    }
                }
            }
        }
    }
    clicked
}

/// A message body with the earlier messages it carries (from block `fold` on, see
/// [`html::quoted_tail`]) folded behind "Show quoted text", which `shown` remembers. Returns the link clicked.
pub fn reply_body(ui: &mut Ui, t: &Tokens, blocks: &[Block], fold: Option<usize>, shown: &mut bool) -> Option<String> {
    let Some(at) = fold.filter(|&at| at < blocks.len()) else {
        return body(ui, t, blocks);
    };
    // Each part in a scope of its own: the body's tight line spacing would shrink the button.
    let mut clicked = ui.scope(|ui| body(ui, t, &blocks[..at])).inner;
    ui.add_space(4.0);
    let (label, tip) = if *shown { ("Hide quoted text", "Fold the earlier messages away again") } else { ("Show quoted text", "The earlier messages this one carries below it") };
    if ui.button(label).on_hover_text(tip).clicked() {
        *shown = !*shown;
    }
    if *shown {
        ui.add_space(12.0);
        clicked = ui.scope(|ui| body(ui, t, &blocks[at..])).inner.or(clicked);
    }
    clicked
}

/// A table of data: columns as wide as their longest cell asks, within the reader's width, a line
/// under the header and between rows. Returns the link clicked.
fn table(ui: &mut Ui, t: &Tokens, rows: &[Vec<Vec<Run>>], header: bool) -> Option<String> {
    const GAP: f32 = 16.0;
    const MIN_W: f32 = 48.0;
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let length = |cell: &[Run]| cell.iter().map(|r| match r { Run::Text { text, .. } | Run::Link { text, .. } => text.chars().count() }).sum::<usize>();
    let wants: Vec<f32> = (0..columns).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|cell| length(cell)).max().unwrap_or(0).max(4) as f32).collect();
    let room = (ui.available_width() - GAP * (columns.saturating_sub(1)) as f32).max(MIN_W * columns as f32);
    let total: f32 = wants.iter().sum();
    let widths: Vec<f32> = wants.iter().map(|w| (room * w / total).max(MIN_W)).collect();
    let mut clicked = None;
    let line = egui::Stroke::new(1.0, t.border);
    for (i, row) in rows.iter().enumerate() {
        let shown = ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (cell, width) in row.iter().zip(&widths) {
                ui.allocate_ui_with_layout(vec2(*width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(*width);
                    let color = if header && i == 0 { t.text_dim } else { t.text };
                    clicked = runs_ui(ui, t, cell, 15.0, color, false, false).or(clicked.take());
                });
            }
        });
        let r = shown.response.rect;
        if i + 1 < rows.len() {
            let stroke = if header && i == 0 { egui::Stroke::new(1.0, t.border_strong) } else { line };
            ui.painter().line_segment([pos2(r.left(), r.bottom() + 3.0), pos2(r.left() + widths.iter().sum::<f32>() + GAP * (columns.saturating_sub(1)) as f32, r.bottom() + 3.0)], stroke);
            ui.add_space(6.0);
        }
    }
    clicked
}

/// What the user did to an attachment's chip.
#[derive(Clone, Copy)]
pub enum ChipAction {
    Open,
    SaveAs,
    /// Ticked or unticked, to save several together.
    Toggle,
}

/// One attached file: its name and size, with Open and Save as…; a tick box when there are several.
/// `ticked` is `None` when the message has a single attachment.
pub fn attachment_chip(ui: &mut Ui, t: &Tokens, file: &Attachment, ticked: Option<bool>) -> Option<ChipAction> {
    let mut chosen = None;
    egui::Frame::new()
        .fill(t.input)
        .stroke(egui::Stroke::new(1.0, t.border_strong))
        .corner_radius(CornerRadius::same(CONTROL_RADIUS))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(mut on) = ticked
                    && ui.checkbox(&mut on, "").on_hover_text("Select to save several together").changed()
                {
                    chosen = Some(ChipAction::Toggle);
                }
                ui.add(Icon::Paperclip.image(icons::SIZE, t.text_dim));
                let name = crate::attachments::safe_name(&file.name);
                let shown: String = if name.chars().count() > 32 { format!("{}…", name.chars().take(31).collect::<String>()) } else { name };
                let open = ui.add(egui::Label::new(egui::RichText::new(shown).color(t.text)).sense(Sense::click())).on_hover_text(format!("{}  ·  {}", file.name, file.mime)).on_hover_cursor(egui::CursorIcon::PointingHand);
                if open.clicked() {
                    chosen = Some(ChipAction::Open);
                }
                ui.label(egui::RichText::new(crate::attachments::human_size(file.size)).size(13.0).color(t.text_dim));
                if ui.button("Open").clicked() {
                    chosen = Some(ChipAction::Open);
                }
                if ui.button("Save as…").clicked() {
                    chosen = Some(ChipAction::SaveAs);
                }
            });
        });
    chosen
}

/// What the user did to a draft row.
pub enum DraftAction {
    None,
    Open,
    Delete,
    /// A scheduled one goes now.
    SendNow,
    /// A scheduled one goes back to being a draft.
    Unschedule,
}

/// "just now", "3 min ago", "2 h ago", "5 d ago".
pub fn ago(then: i64, now: i64) -> String {
    let seconds = (now - then).max(0);
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// A draft kept on this computer: who it is for, the subject and the start of the text.
pub fn draft_row(ui: &mut Ui, t: &Tokens, draft: &crate::drafts::DraftData) -> DraftAction {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), MESSAGE_ROW_H), Sense::click());
    let card = rect.shrink2(vec2(8.0, 2.0));
    row_background(ui, card, &response, false, t);
    let center = pos2(card.left() + 14.0 + AVATAR / 2.0, card.top() + 14.0 + AVATAR / 2.0);
    ui.painter().circle_filled(center, AVATAR / 2.0, t.accent.gamma_multiply(0.18));
    Icon::Pencil.image(icons::SIZE, t.accent).paint_at(ui, Rect::from_center_size(center, vec2(icons::SIZE, icons::SIZE)));
    let left = card.left() + 14.0 + AVATAR + 12.0;
    let width = card.right() - 14.0 - left;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (when, color) = match (draft.queued, draft.send_at) {
        (true, Some(at)) => (format!("Scheduled for {}", crate::compose::when_text(at)), t.accent),
        (true, None) => ("Waiting to send".to_owned(), t.accent),
        _ => (ago(draft.updated, now), t.text_dim),
    };
    let when = ui.painter().layout_no_wrap(when, FontId::proportional(13.0), color);
    let when_w = when.size().x;
    ui.painter().galley(pos2(card.right() - 14.0 - when_w, card.top() + 13.0), when, color);
    let to = if draft.to.trim().is_empty() { "No recipient yet".to_owned() } else { draft.to.clone() };
    let who = truncated(ui, &to, FontId::proportional(15.5), t.text, width - when_w - 12.0);
    ui.painter().galley(pos2(left, card.top() + 11.0), who, t.text);
    let subject = if draft.subject.trim().is_empty() { "(no subject)" } else { draft.subject.trim() };
    let subject = truncated(ui, subject, FontId::proportional(14.5), t.text, width);
    ui.painter().galley(pos2(left, card.top() + 33.0), subject, t.text);
    let start = draft.body.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('>')).unwrap_or("");
    let start = truncated(ui, start, FontId::proportional(13.5), t.text_dim, width);
    ui.painter().galley(pos2(left, card.top() + 56.0), start, t.text_dim);
    let mut action = if response.clicked() { DraftAction::Open } else { DraftAction::None };
    context_menu(&response.on_hover_cursor(egui::CursorIcon::PointingHand), |ui| {
        if draft.queued && draft.send_at.is_some() {
            if ui.button("Send now").clicked() {
                action = DraftAction::SendNow;
                ui.close();
            }
            if ui.button("Cancel schedule").on_hover_text("Keep it as a draft, not sent").clicked() {
                action = DraftAction::Unschedule;
                ui.close();
            }
        }
        if ui.button("Delete draft").clicked() {
            action = DraftAction::Delete;
            ui.close();
        }
    });
    action
}

/// A row of the sidebar that is not a folder: its icon, title and count.
pub fn icon_row(ui: &mut Ui, t: &Tokens, title: &str, icon: Icon, count: u32, selected: bool) -> Response {
    nav_row(ui, t, title, Some((icon, if selected { t.accent } else { t.text_dim })), count, selected)
}

/// 1240 as "1,240".
pub fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{signature_line, thousands, trackers_line};

    #[test]
    fn a_folders_place_reads_as_words() {
        use beavermail_core::Role;
        assert_eq!(super::folder_path("INBOX/Receipts", Role::Other, Some('/')), "Inbox › Receipts");
        assert_eq!(super::folder_path("INBOX.Receipts.Old", Role::Other, Some('.')), "Inbox › Receipts › Old");
        assert_eq!(super::folder_path("Hobby/Citt&AOA-", Role::Other, None), "Hobby › Città");
        assert_eq!(super::folder_path("INBOX", Role::Inbox, Some('/')), "Inbox");
        // With "/" as the separator, a dot is part of the name.
        assert_eq!(super::folder_path("Projects/v1.0", Role::Other, Some('/')), "Projects › v1.0");
        assert_eq!(super::folder_title("Projects/v1.0", Role::Other, Some('/')), "v1.0");
        assert!(super::is_inside("A/B", "A", Some('/')) && !super::is_inside("A.B", "A", Some('/')) && super::is_inside("A.B", "A", None));
        assert!(!super::is_inside("AB", "A", None) && !super::is_inside("A/", "A", Some('/')));
    }

    #[test]
    fn a_folder_sits_under_the_one_it_is_inside_and_role_folders_stay_at_the_top() {
        use beavermail_core::{Mailbox, Role};
        let folder = |name: &str, role| Mailbox { name: name.into(), unread: 0, role, total: 0, stored: 0, delimiter: Some('.') };
        let mailboxes = [
            folder("INBOX.Trash", Role::Trash),
            folder("INBOX.Zeta", Role::Other),
            folder("INBOX", Role::Inbox),
            folder("INBOX.Receipts.Old", Role::Other),
            folder("INBOX.Sent", Role::Sent),
            folder("INBOX.Receipts", Role::Other),
            folder("Hobby.Garden", Role::Other),
            folder("Hobby", Role::Other),
            // The separator is "." here: a "/" is part of the name.
            folder("Hobby/Notes", Role::Other),
            folder("INBOX.alpha", Role::Other),
        ];
        let shown: Vec<(&str, usize)> = super::folder_tree(&mailboxes).into_iter().map(|(i, depth)| (mailboxes[i].name.as_str(), depth)).collect();
        assert_eq!(
            shown,
            [
                ("INBOX", 0),
                ("INBOX.alpha", 1),
                ("INBOX.Receipts", 1),
                ("INBOX.Receipts.Old", 2),
                ("INBOX.Zeta", 1),
                ("INBOX.Sent", 0),
                ("INBOX.Trash", 0),
                ("Hobby", 0),
                ("Hobby.Garden", 1),
                ("Hobby/Notes", 0),
            ]
        );
    }

    #[test]
    fn encryption_and_a_good_signature_share_one_line_and_problems_get_their_own() {
        use beavermail_core::{Encryption, Signature, SignatureCheck, SignedWith};
        use super::{Tone, security_notes};
        let opened = Encryption::Opened(SignedWith::OpenPgp);
        let good = Signature { kind: SignedWith::OpenPgp, check: SignatureCheck::Valid { signer: "ada@example.org".into(), by: "key 1AD8".into() } };
        let notes = security_notes(Some(&opened), Some(&good));
        assert_eq!(notes.len(), 1);
        assert_eq!((notes[0].0.as_str(), notes[0].2), ("Encrypted  ·  Signed by ada@example.org", Tone::Good));
        assert!(notes[0].1.contains("only you and the other recipients") && notes[0].1.contains("key 1AD8"), "the details on hover");
        let changed = Signature { kind: SignedWith::OpenPgp, check: SignatureCheck::Changed };
        let notes = security_notes(Some(&opened), Some(&changed));
        assert_eq!(notes.iter().map(|n| n.2).collect::<Vec<_>>(), [Tone::Good, Tone::Warning], "the problem spelled out");
        assert_eq!(security_notes(Some(&Encryption::NoKey(SignedWith::Smime)), None)[0].2, Tone::Quiet);
        assert!(security_notes(None, None).is_empty());
    }

    #[test]
    fn a_signature_reads_as_one_line_and_warns_when_it_must() {
        use beavermail_core::{Signature, SignatureCheck, SignedWith};
        let line = |kind, check| signature_line(&Signature { kind, check });
        assert_eq!(line(SignedWith::Smime, SignatureCheck::Valid { signer: "ada@example.org".into(), by: "Test CA".into() }), ("Signed by ada@example.org  ·  certificate from Test CA (S/MIME)".to_owned(), false));
        assert!(line(SignedWith::OpenPgp, SignatureCheck::Changed).1);
        assert!(line(SignedWith::OpenPgp, SignatureCheck::KeyChanged { signer: "ada@example.org".into() }).1);
        assert!(!line(SignedWith::OpenPgp, SignatureCheck::UnknownKey { key: "AB12".into() }).1, "unknown is not a warning: most keys are");
    }

    #[test]
    fn trackers_name_each_host_once() {
        let urls: Vec<String> = ["https://b.example/o.gif", "https://a.example/1", "https://b.example/o2.gif"].map(String::from).to_vec();
        assert_eq!(trackers_line(&urls).as_deref(), Some("3 trackers blocked: a.example, b.example"));
        assert_eq!(trackers_line(&urls[..1]).as_deref(), Some("1 tracker blocked: b.example"));
        assert_eq!(trackers_line(&[]), None);
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!([(0, 30), (0, 120), (0, 7200), (0, 200_000)].map(|(a, b)| super::ago(a, b)), ["just now", "2 min ago", "2 h ago", "2 d ago"]);
        assert_eq!(super::ago(500, 100), "just now", "a clock in the past is not negative");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!([0, 12, 999, 1_000, 1_240, 50_000, 1_234_567].map(thousands), ["0", "12", "999", "1,000", "1,240", "50,000", "1,234,567"]);
    }
}
