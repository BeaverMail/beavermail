// SPDX-License-Identifier: AGPL-3.0-only

//! The Tasks section: the task lists of CalDAV servers and the views across them (Today, Overdue,
//! Next 7 days, All); a list's open tasks grouped by when they are due, each with its subtasks under
//! it; and the task picked, changed in place.

use std::collections::{HashMap, HashSet};

use beavermail_carddav::ical::When;
use beavermail_carddav::todo::{Edit, Task};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, FontId, Layout, Rect, RichText, Sense, pos2, vec2};

use crate::icons::{self, Icon};
use crate::theme::{self, Tokens};
use crate::views;

/// What the list shows: a view across every list, or one list (by URL).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    Today,
    Overdue,
    Week,
    All,
    List(String),
}

impl Filter {
    pub fn title(&self) -> &str {
        match self {
            Self::Today => "Today",
            Self::Overdue => "Overdue",
            Self::Week => "Next 7 days",
            Self::All => "All tasks",
            Self::List(_) => "",
        }
    }
}

/// A task as the section shows it: where it is kept, and whether it can be changed there.
#[derive(Clone, Debug)]
pub struct Shown {
    pub id: i64,
    pub list: String,
    pub task: Task,
    pub writable: bool,
}

/// A task list as the column shows it.
#[derive(Clone, Debug)]
pub struct List {
    pub url: String,
    pub name: String,
    /// The server it is on, as Settings names it.
    pub server: String,
    pub color: Color32,
    pub writable: bool,
    pub default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Overdue,
    Today,
    Upcoming,
    NoDate,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Self::Overdue => "Overdue",
            Self::Today => "Today",
            Self::Upcoming => "Upcoming",
            Self::NoDate => "No date",
        }
    }
}

/// A row of the list: the task (its place in the tasks shown), how deep under its parent, and how
/// many subtasks it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub at: usize,
    pub depth: usize,
    pub children: usize,
}

/// When a task is due, here: its day, and its time when it has one.
pub fn due(task: &Task) -> Option<(NaiveDate, Option<NaiveTime>)> {
    let d = task.due.as_ref()?;
    if d.date_only {
        return Some((NaiveDate::parse_from_str(d.value.get(..8)?, "%Y%m%d").ok()?, None));
    }
    let here = d.utc()?.with_timezone(&Local).naive_local();
    Some((here.date(), Some(here.time())))
}

/// The date (and time) written for a task due on `day`, at `time` when given: a date alone, or the
/// moment in UTC.
pub fn due_when(day: NaiveDate, time: Option<NaiveTime>) -> When {
    match time.and_then(|time| Local.from_local_datetime(&day.and_time(time)).earliest()) {
        Some(at) => When { value: at.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string(), tzid: None, date_only: false },
        None => When { value: day.format("%Y%m%d").to_string(), tzid: None, date_only: true },
    }
}

pub fn group(task: &Task, now: NaiveDateTime) -> Group {
    match due(task) {
        None => Group::NoDate,
        Some((day, time)) if day < now.date() || (day == now.date() && time.is_some_and(|t| t < now.time())) => Group::Overdue,
        Some((day, _)) if day == now.date() => Group::Today,
        Some(_) => Group::Upcoming,
    }
}

fn wanted(filter: &Filter, s: &Shown, now: NaiveDateTime) -> bool {
    let g = group(&s.task, now);
    match filter {
        Filter::Today => matches!(g, Group::Overdue | Group::Today),
        Filter::Overdue => g == Group::Overdue,
        Filter::Week => g != Group::NoDate && due(&s.task).is_some_and(|(day, _)| day < now.date() + Duration::days(7)),
        Filter::All => true,
        Filter::List(url) => s.list == *url,
    }
}

/// 1 (high) first, then 5, then 9, then none.
fn priority_rank(p: u8) -> u8 {
    if p == 0 { 10 } else { p }
}

/// The rows a list shows: the open tasks of `filter` (and those in `keep`, just ticked), by group,
/// each followed by its subtasks unless `folded` (by `UID`). A subtask whose parent is not shown
/// stands on its own; a loop of parents is broken at the task met first.
pub fn lines(shown: &[Shown], filter: &Filter, now: NaiveDateTime, keep: &HashSet<i64>, folded: &HashSet<String>) -> Vec<(Group, Vec<Line>)> {
    let open: Vec<usize> = (0..shown.len()).filter(|&i| (!shown[i].task.done || keep.contains(&shown[i].id)) && wanted(filter, &shown[i], now)).collect();
    let by_uid: HashMap<&str, usize> = open.iter().map(|&i| (shown[i].task.uid.as_str(), i)).collect();
    let mut kids: HashMap<&str, Vec<usize>> = HashMap::new();
    for &i in &open {
        if let Some(parent) = shown[i].task.parent.as_deref().filter(|p| by_uid.contains_key(p) && *p != shown[i].task.uid) {
            kids.entry(parent).or_default().push(i);
        }
    }
    for list in kids.values_mut() {
        list.sort_by_key(|&i| (shown[i].task.order(), shown[i].id));
    }
    let key = |i: usize| order_key(&shown[i]);
    let mut roots: Vec<usize> = open.iter().copied().filter(|&i| shown[i].task.parent.as_deref().is_none_or(|p| !by_uid.contains_key(p) || p == shown[i].task.uid)).collect();
    roots.sort_by_key(|&i| key(i));
    let mut seen = HashSet::new();
    let mut out: Vec<(Group, Vec<Line>)> = Vec::new();
    let walk = |root: usize, out: &mut Vec<(Group, Vec<Line>)>, seen: &mut HashSet<usize>| {
        let g = group(&shown[root].task, now);
        if out.last().is_none_or(|(last, _)| *last != g) {
            out.push((g, Vec::new()));
        }
        let mut stack = vec![(root, 0)];
        while let Some((i, depth)) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            let children: Vec<usize> = kids.get(shown[i].task.uid.as_str()).cloned().unwrap_or_default();
            if let Some((_, rows)) = out.last_mut() {
                rows.push(Line { at: i, depth, children: children.len() });
            }
            if !folded.contains(&shown[i].task.uid) {
                stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
            }
        }
    };
    for root in roots {
        walk(root, &mut out, &mut seen);
    }
    // Only loops are left: each is shown from the first of its tasks met.
    let mut rest: Vec<usize> = open.iter().copied().filter(|i| !seen.contains(i)).collect();
    rest.sort_by_key(|&i| key(i));
    for root in rest {
        if !seen.contains(&root) && !folded_away(shown, root, &by_uid, folded) {
            walk(root, &mut out, &mut seen);
        }
    }
    // Groups in their order, each once: a loop shown last may add to one already there.
    out.sort_by_key(|(g, _)| *g);
    let mut merged: Vec<(Group, Vec<Line>)> = Vec::new();
    for (g, rows) in out {
        match merged.last_mut() {
            Some((last, kept)) if *last == g => kept.extend(rows),
            _ => merged.push((g, rows)),
        }
    }
    merged
}

/// Where a task goes among the others: by day, then time (one without comes at the end of its day),
/// then the order put by hand among those due alike, then priority, then when it was made.
fn order_key(s: &Shown) -> (Option<NaiveDate>, NaiveTime, i64, u8, i64, i64) {
    let t = &s.task;
    let (day, time) = due(t).map_or((None, None), |(d, time)| (Some(d), time));
    let end = NaiveTime::from_hms_opt(23, 59, 59).unwrap_or_default();
    (day, time.unwrap_or(end), t.sort.unwrap_or(i64::MAX), priority_rank(t.priority), t.order(), s.id)
}

/// The tasks that can be put in order by hand among themselves: open ones due alike (the same day and
/// time, or no date), not under a parent.
pub fn same_slot(a: &Shown, b: &Shown) -> bool {
    due(&a.task) == due(&b.task)
}

/// A task of a loop of parents whose parent is folded: hidden with it.
fn folded_away(shown: &[Shown], i: usize, by_uid: &HashMap<&str, usize>, folded: &HashSet<String>) -> bool {
    shown[i].task.parent.as_deref().is_some_and(|p| by_uid.contains_key(p) && folded.contains(p))
}

/// The done tasks of a list (or of all of them), the latest done first.
pub fn completed(shown: &[Shown], filter: &Filter) -> Vec<usize> {
    let mut done: Vec<usize> = (0..shown.len()).filter(|&i| shown[i].task.done && matches!(filter, Filter::All) || matches!(filter, Filter::List(url) if *url == shown[i].list && shown[i].task.done)).collect();
    done.sort_by_key(|&i| std::cmp::Reverse(shown[i].task.completed.as_ref().and_then(When::utc).map_or(0, |t| t.timestamp())));
    done
}

/// "Today", "Tomorrow 15:00", "Mon 7 Dec", "Yesterday": when a task is due, as a row says it.
pub fn due_label(task: &Task, today: NaiveDate) -> Option<String> {
    let (day, time) = due(task)?;
    let name = match (day - today).num_days() {
        0 => "Today".to_owned(),
        1 => "Tomorrow".to_owned(),
        -1 => "Yesterday".to_owned(),
        2..=6 => day.format("%A").to_string(),
        _ if day.year() == today.year() => day.format("%a %-d %b").to_string(),
        _ => day.format("%-d %b %Y").to_string(),
    };
    Some(match time {
        Some(t) => format!("{name} {}", t.format("%H:%M")),
        None => name,
    })
}

/// What the section asks for.
#[derive(Clone, Debug)]
pub enum TaskAction {
    Pick(Filter),
    Open(i64),
    Close,
    /// Tick a task done, or open it again.
    Tick(i64),
    /// A new task with this title: in `list` (else the default one), under the task with `parent` as `UID`.
    Add { title: String, list: Option<String>, parent: Option<String> },
    Change(i64, Vec<Edit>),
    /// A task dragged onto another (by id), to go above it or `after` it; made into [`TaskAction::Reorder`].
    Drop { moved: i64, onto: i64, after: bool },
    /// Tasks due alike, in the order put by hand.
    Reorder(Vec<i64>),
    /// Move a task (with its subtasks) to the list at this URL.
    Move(i64, String),
    Delete(i64),
    /// Delete the done tasks of the list at this URL, after a confirmation.
    DeleteCompleted(String),
    /// Fold or unfold a task's subtasks, by its `UID`.
    Fold(String),
    /// Show or hide the done tasks at the bottom of the list.
    ShowCompleted,
    /// Open the message a task was made from, by its Message-ID.
    OpenMessage(String),
    /// Open the dialog to make a list on a server.
    NewList,
    /// Tasks made here go to this list (by URL).
    MakeDefault(String),
    SyncNow,
}

/// The column on the left: the views across every list, with how many tasks each has, then the lists
/// under the server they are on, each with its colour and open tasks.
pub fn sidebar(ui: &mut egui::Ui, t: &Tokens, filter: &Filter, lists: &[List], counts: [u32; 4], open: &HashMap<String, u32>) -> Option<TaskAction> {
    let mut action = None;
    for (f, icon, count) in [(Filter::Today, Icon::Calendar, counts[0]), (Filter::Overdue, Icon::CircleAlert, counts[1]), (Filter::Week, Icon::Calendar, counts[2]), (Filter::All, Icon::ListChecks, counts[3])] {
        if views::icon_row(ui, t, f.title(), icon, count, *filter == f).clicked() {
            action = Some(TaskAction::Pick(f));
        }
    }
    let mut servers: Vec<&str> = Vec::new();
    for l in lists {
        if !servers.contains(&l.server.as_str()) {
            servers.push(&l.server);
        }
    }
    for server in servers {
        ui.add_space(14.0);
        ui.label(RichText::new(server).size(13.0).color(t.text_dim));
        ui.add_space(2.0);
        for l in lists.iter().filter(|l| l.server == server) {
            let name = if l.default { format!("{}  ·  default", l.name) } else { l.name.clone() };
            let selected = matches!(filter, Filter::List(url) if *url == l.url);
            let row = views::dot_row(ui, t, &name, l.color, open.get(&l.url).copied().unwrap_or(0), selected);
            if row.clicked() {
                action = Some(TaskAction::Pick(Filter::List(l.url.clone())));
            }
            views::context_menu(&row, |ui| {
                let why = if l.writable { "Tasks made here already go to this list" } else { "Read-only: nothing can be written there" };
                if ui.add_enabled(!l.default && l.writable, egui::Button::new("Make default")).on_disabled_hover_text(why).clicked() {
                    action = Some(TaskAction::MakeDefault(l.url.clone()));
                    ui.close();
                }
                if ui.button("Sync now").clicked() {
                    action = Some(TaskAction::SyncNow);
                    ui.close();
                }
            });
        }
    }
    action
}

/// What the list column keeps between frames.
#[derive(Default)]
pub struct ListState {
    pub quick: String,
    pub show_completed: bool,
    /// The task picked moved by a key: scrolled into sight once.
    pub reveal: bool,
}

/// The middle column: the title, a field to add a task, the open tasks by group, and at the bottom of
/// a list its done tasks, folded. `keep`: tasks just ticked, still shown struck through.
#[allow(clippy::too_many_arguments)]
pub fn list(ui: &mut egui::Ui, t: &Tokens, filter: &Filter, title: &str, shown: &[Shown], lists: &[List], state: &mut ListState, picked: Option<i64>, keep: &HashSet<i64>, folded: &HashSet<String>, can_add: Result<(), &str>) -> Option<TaskAction> {
    let mut action = None;
    let now = Local::now().naive_local();
    let today = now.date();
    let rows = lines(shown, filter, now, keep, folded);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(theme::SUBTITLE + 3.0).family(theme::bold()).color(t.text));
    });
    ui.add_space(10.0);
    let hint = match filter {
        Filter::Today => "Add a task for today  (N)",
        _ => "Add a task  (N)",
    };
    let field = ui.add_enabled(can_add.is_ok(), crate::sized_field(&mut state.quick, ui.available_width()).hint_text(hint).id(egui::Id::new(QUICK)));
    let field = field.on_disabled_hover_text(can_add.err().unwrap_or_default());
    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !state.quick.trim().is_empty() {
        let list = if let Filter::List(url) = filter { Some(url.clone()) } else { None };
        action = Some(TaskAction::Add { title: std::mem::take(&mut state.quick).trim().to_owned(), list, parent: None });
        field.request_focus();
    }
    ui.add_space(6.0);
    let in_one_list = matches!(filter, Filter::List(_));
    let done = completed(shown, filter);
    let dragged = egui::DragAndDrop::payload::<i64>(ui.ctx()).map(|p| *p);
    let reveal = std::mem::take(&mut state.reveal);
    egui::ScrollArea::vertical().id_salt("tasks").auto_shrink([false, false]).show(ui, |ui| {
        for (g, group_rows) in &rows {
            ui.add_space(12.0);
            ui.label(RichText::new(g.title()).family(theme::bold()).color(if *g == Group::Overdue { t.danger } else { t.text }));
            ui.add_space(2.0);
            for line in group_rows {
                let s = &shown[line.at];
                let list_name = (!in_one_list).then(|| lists.iter().find(|l| l.url == s.list)).flatten();
                // Put in order by hand among the open tasks due alike, not under a parent.
                let movable = line.depth == 0 && !s.task.done && s.writable;
                let accepts = movable && dragged.is_some_and(|d| d != s.id && shown.iter().find(|o| o.id == d).is_some_and(|o| same_slot(o, s)));
                let look = Look { selected: picked == Some(s.id), folded: folded.contains(&s.task.uid), movable, accepts, reveal: reveal && picked == Some(s.id) };
                match row(ui, t, s, *line, list_name, today, look) {
                    Some(TaskAction::Drop { moved, onto, after }) => {
                        let mut order: Vec<i64> = group_rows.iter().map(|l| &shown[l.at]).filter(|o| o.id != moved && same_slot(o, s) && !o.task.done).map(|o| o.id).collect();
                        let at = order.iter().position(|id| *id == onto).map_or(order.len(), |p| p + usize::from(after));
                        order.insert(at, moved);
                        action = Some(TaskAction::Reorder(order));
                    }
                    Some(a) => action = Some(a),
                    None => {}
                }
            }
        }
        if rows.is_empty() {
            let (title, hint) = match filter {
                Filter::Today => ("Nothing due today", "Tasks due today and overdue ones show here."),
                Filter::Overdue => ("Nothing overdue", "Tasks past their due date show here."),
                Filter::Week => ("Nothing in the next 7 days", "Tasks due this week show here."),
                _ => ("No open tasks", "Type a task above and press Enter."),
            };
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(theme::SUBTITLE).family(theme::bold()).color(t.text));
                ui.label(RichText::new(hint).color(t.text_dim));
            });
        }
        if !done.is_empty() {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let arrow = if state.show_completed { Icon::ChevronDown } else { Icon::ChevronRight };
                let label = format!("Completed ({})", done.len());
                if ui.add(egui::Button::image_and_text(arrow.image(icons::SIZE, t.text_dim), RichText::new(label).color(t.text_dim)).frame(false)).clicked() {
                    action = Some(TaskAction::ShowCompleted);
                }
                if let Filter::List(url) = filter
                    && state.show_completed
                    && shown.iter().any(|s| s.list == *url && s.writable)
                {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(RichText::new("Delete completed…").color(t.danger)).clicked() {
                            action = Some(TaskAction::DeleteCompleted(url.clone()));
                        }
                    });
                }
            });
            if state.show_completed {
                for &i in &done {
                    let s = &shown[i];
                    let list_name = (!in_one_list).then(|| lists.iter().find(|l| l.url == s.list)).flatten();
                    let look = Look { selected: picked == Some(s.id), reveal: reveal && picked == Some(s.id), ..Look::default() };
                    if let Some(a) = row(ui, t, s, Line { at: i, depth: 0, children: 0 }, list_name, today, look) {
                        action = Some(a);
                    }
                }
            }
        }
    });
    action
}

/// Room for the round box to tick a task, at the left of its row.
const TICK: f32 = 36.0;

/// The id of the field to add a task, for the key that goes to it.
pub const QUICK: &str = "task_quick";

/// How a row is drawn: picked, its subtasks folded, whether it can be dragged into another place and
/// takes the task being dragged, and whether to scroll it into sight.
#[derive(Clone, Copy, Default)]
struct Look {
    selected: bool,
    folded: bool,
    movable: bool,
    accepts: bool,
    reveal: bool,
}

/// One task: the box to tick it, its title (struck through once done), and a dim line with when it
/// is due (in red past it), its list in a view across lists, its labels and what else it has.
fn row(ui: &mut egui::Ui, t: &Tokens, s: &Shown, line: Line, list: Option<&List>, today: NaiveDate, look: Look) -> Option<TaskAction> {
    let Look { selected, folded, movable, accepts, reveal } = look;
    let task = &s.task;
    let mut action = None;
    let mut meta: Vec<(String, Color32)> = Vec::new();
    if let Some(label) = due_label(task, today) {
        let late = !task.done && group(task, Local::now().naive_local()) == Group::Overdue;
        meta.push((label, if late { t.danger } else { t.text_dim }));
    }
    if let Some(l) = list {
        meta.push((l.name.clone(), t.text_dim));
    }
    meta.extend(task.categories.iter().map(|c| (format!("#{c}"), t.text_dim)));
    if line.children > 0 {
        meta.push((if line.children == 1 { "1 subtask".to_owned() } else { format!("{} subtasks", line.children) }, t.text_dim));
    }
    let height = if meta.is_empty() { 40.0 } else { 52.0 };
    let indent = 24.0 * line.depth as f32;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), if movable { Sense::click_and_drag() } else { Sense::click() });
    if reveal {
        response.scroll_to_me(Some(Align::Center));
    }
    let being_dragged = response.dragged();
    let fill = if selected { t.accent.gamma_multiply(0.14) } else if response.hovered() || being_dragged { t.hover } else { Color32::TRANSPARENT };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::CONTROL_RADIUS), fill);
    if movable {
        response.dnd_set_drag_payload(s.id);
    }
    // Where the task dragged would go: a line above or below this one.
    if accepts && let Some(pointer) = response.dnd_hover_payload::<i64>().and(ui.ctx().pointer_interact_pos()) {
        let after = pointer.y > rect.center().y;
        let y = if after { rect.bottom() } else { rect.top() };
        ui.painter().line_segment([pos2(rect.left() + 8.0, y), pos2(rect.right() - 8.0, y)], egui::Stroke::new(2.0, t.accent));
    }
    if accepts
        && let Some(moved) = response.dnd_release_payload::<i64>()
        && let Some(pointer) = ui.ctx().pointer_interact_pos()
    {
        action = Some(TaskAction::Drop { moved: *moved, onto: s.id, after: pointer.y > rect.center().y });
    }
    // The box: its own target, so ticking does not open the task.
    let tick = Rect::from_min_size(pos2(rect.left() + indent + 4.0, rect.top() + (height - TICK) / 2.0), vec2(TICK, TICK));
    if tick_box(ui, t, tick, s, 9.0).clicked() && s.writable {
        action = Some(TaskAction::Tick(s.id));
    }
    let left = tick.right() + 4.0;
    let mut right = rect.right() - 8.0;
    if line.children > 0 {
        let fold = Rect::from_center_size(pos2(right - 14.0, rect.center().y), vec2(28.0, 28.0));
        let fold_response = ui.interact(fold, ui.id().with(("fold", s.id)), Sense::click()).on_hover_text(if folded { "Show subtasks" } else { "Hide subtasks" });
        if fold_response.hovered() {
            ui.painter().rect_filled(fold, CornerRadius::same(6), t.pressed);
        }
        let icon = if folded { Icon::ChevronRight } else { Icon::ChevronDown };
        icon.image(16.0, t.text_dim).paint_at(ui, Rect::from_center_size(fold.center(), vec2(16.0, 16.0)));
        if fold_response.clicked() {
            action = Some(TaskAction::Fold(task.uid.clone()));
        }
        right = fold.left() - 4.0;
    }
    for (show, icon, tip) in [(task.repeats(), Icon::Repeat, "Repeats"), (task.message_id().is_some(), Icon::Mail, "Made from a message")] {
        if show {
            let at = Rect::from_center_size(pos2(right - 9.0, rect.center().y), vec2(16.0, 16.0));
            icon.image(16.0, t.text_dim).paint_at(ui, at);
            ui.interact(at, ui.id().with((tip, s.id)), Sense::hover()).on_hover_text(tip);
            right = at.left() - 8.0;
        }
    }
    let room = (right - left).max(40.0);
    let title = if task.summary.trim().is_empty() { "(no title)" } else { task.summary.as_str() };
    let color = if task.done { t.text_dim } else { t.text };
    let galley = views::truncated(ui, title, FontId::proportional(15.0), color, room);
    let top = if meta.is_empty() { rect.center().y - galley.size().y / 2.0 } else { rect.top() + 7.0 };
    let size = galley.size();
    ui.painter().galley(pos2(left, top), galley, color);
    if task.done {
        let y = top + size.y / 2.0;
        ui.painter().line_segment([pos2(left, y), pos2(left + size.x, y)], egui::Stroke::new(1.0, t.text_dim));
    }
    if !meta.is_empty() {
        let mut x = left;
        for (i, (text, color)) in meta.iter().enumerate() {
            let text = if i == 0 { text.clone() } else { format!("·  {text}") };
            let g = views::truncated(ui, &text, FontId::proportional(13.0), *color, (left + room - x).max(0.0));
            let w = g.size().x;
            ui.painter().galley(pos2(x, rect.top() + 29.0), g, *color);
            x += w + 6.0;
            if x > left + room {
                break;
            }
        }
    }
    let cursor = if being_dragged { egui::CursorIcon::Grabbing } else { egui::CursorIcon::PointingHand };
    let response = response.on_hover_cursor(cursor);
    if response.clicked() && action.is_none() {
        action = Some(TaskAction::Open(s.id));
    }
    views::context_menu(&response, |ui| {
        let today = Local::now().date_naive();
        let next_monday = today + Duration::days(i64::from(7 - today.weekday().num_days_from_monday()));
        for (label, day) in [("Today", Some(today)), ("Tomorrow", Some(today + Duration::days(1))), ("Next week", Some(next_monday)), ("No date", None)] {
            if ui.add_enabled(s.writable, egui::Button::new(label)).clicked() {
                let when = day.map(|d| due_when(d, None));
                action = Some(TaskAction::Change(s.id, vec![Edit::due(task, when.as_ref())]));
                ui.close();
            }
        }
        ui.separator();
        if ui.add_enabled(s.writable, egui::Button::new(RichText::new("Delete").color(t.danger))).clicked() {
            action = Some(TaskAction::Delete(s.id));
            ui.close();
        }
    });
    action
}

/// The round box to tick a task, in `rect`, of `radius`: filled with a check once done, its ring
/// coloured by priority (high red, medium accent). Its own target, so ticking does not open the task.
fn tick_box(ui: &mut egui::Ui, t: &Tokens, rect: Rect, s: &Shown, radius: f32) -> egui::Response {
    let task = &s.task;
    let response = ui.interact(rect, ui.id().with(("tick", s.id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    let tip = if !s.writable { "Read-only list" } else if task.done { "Open it again" } else if task.repeats() { "Done: the next one comes" } else { "Mark done" };
    let response = response.on_hover_text(tip);
    let center = rect.center();
    let ring = match task.priority {
        1..=4 => t.danger,
        5 => t.accent,
        _ => t.border_strong,
    };
    if task.done {
        ui.painter().circle_filled(center, radius, t.accent);
        let (k, stroke) = (radius / 9.0, egui::Stroke::new(1.75, t.on_accent));
        ui.painter().line_segment([center + vec2(-4.0, 0.0) * k, center + vec2(-1.0, 3.0) * k], stroke);
        ui.painter().line_segment([center + vec2(-1.0, 3.0) * k, center + vec2(4.5, -3.0) * k], stroke);
    } else {
        if response.hovered() && s.writable {
            ui.painter().circle_filled(center, radius, t.hover);
        }
        ui.painter().circle_stroke(center, radius, egui::Stroke::new(1.5, ring));
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, s.writable, task.done, task.summary.as_str()));
    response
}

/// The task picked as it is being changed: the text typed, kept until the field is left.
#[derive(Clone, Debug, Default)]
pub struct Editing {
    pub id: i64,
    pub title: String,
    pub notes: String,
    pub label: String,
    pub subtask: String,
    /// The field to add a label is out (else a chip stands for it).
    pub labeling: bool,
    /// The month the due picker shows.
    pub month: NaiveDate,
}

impl Editing {
    /// The title and notes typed and not kept yet, once neither field is being typed in: kept then
    /// whatever took the focus away (another task opened, the section left).
    pub fn pending(&self, ctx: &egui::Context, task: &Task) -> Vec<Edit> {
        let typing = ctx.memory(|m| m.has_focus(egui::Id::new(("task_title", self.id))) || m.has_focus(egui::Id::new(("task_notes", self.id))));
        if typing { Vec::new() } else { self.changes(task) }
    }

    /// The title and notes typed that differ from the task's.
    pub fn changes(&self, task: &Task) -> Vec<Edit> {
        let mut out = Vec::new();
        if self.title.trim() != task.summary.trim() {
            out.push(Edit::summary(&self.title));
        }
        if self.notes.trim_end() != task.description.trim_end() {
            out.push(Edit::description(&self.notes));
        }
        out
    }

    pub fn of(s: &Shown) -> Self {
        let day = due(&s.task).map_or_else(|| Local::now().date_naive(), |(d, _)| d);
        Self { id: s.id, title: s.task.summary.clone(), notes: s.task.description.clone(), label: String::new(), subtask: String::new(), labeling: false, month: day.with_day(1).unwrap_or(day) }
    }
}

/// The task picked, changed in place: its box to tick and title, then when due, priority, list and
/// labels side by side with their names, then notes and subtasks; the message it was made from; Delete.
pub fn details(ui: &mut egui::Ui, t: &Tokens, s: &Shown, children: &[&Shown], lists: &[List], edit: &mut Editing) -> Option<TaskAction> {
    let task = &s.task;
    let mut action = None;
    ui.horizontal_top(|ui| {
        let (tick, _) = ui.allocate_exact_size(vec2(TICK, TICK), Sense::hover());
        if tick_box(ui, t, tick, s, 11.0).clicked() && s.writable {
            action = Some(TaskAction::Tick(s.id));
        }
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if icons::button(ui, t, Icon::X, "Close (Esc)", true, false).clicked() {
                action = Some(TaskAction::Close);
            }
            // A title, not a form field: its frame shows only under the pointer or while typing.
            let id = egui::Id::new(("task_title", s.id));
            let color = if task.done { t.text_dim } else { t.text };
            // On as many lines as it needs; Enter ends it rather than starting a line.
            let field = egui::TextEdit::multiline(&mut edit.title).return_key(None).desired_rows(1).font(FontId::new(20.0, theme::bold())).text_color(color).hint_text("(no title)").desired_width(ui.available_width()).frame(egui::Frame::NONE).margin(egui::Margin::symmetric(8, 6)).id(id);
            let response = ui.add_enabled(s.writable, field);
            if edit.title.contains('\n') {
                edit.title = edit.title.replace(['\r', '\n'], " ");
            }
            if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                response.surrender_focus();
            }
            let stroke = if response.has_focus() { Some(t.accent) } else if response.hovered() && s.writable { Some(t.border_strong) } else { None };
            if let Some(color) = stroke {
                ui.painter().rect_stroke(response.rect, CornerRadius::same(theme::CONTROL_RADIUS), egui::Stroke::new(1.0, color), egui::StrokeKind::Inside);
            }
        });
    });
    ui.add_space(16.0);
    let width = crate::FIELD_WIDTH.min(ui.available_width() - 100.0).max(180.0);
    ui.add_enabled_ui(s.writable, |ui| {
        crate::form_grid(ui, "task_grid", |ui| {
            ui.label("Due");
            if let Some(edits) = due_row(ui, t, task, &mut edit.month, width) {
                action = Some(TaskAction::Change(s.id, edits));
            }
            ui.end_row();
            ui.label("Priority");
            let names = [(0u8, "None"), (9, "Low"), (5, "Medium"), (1, "High")];
            let current = match task.priority {
                0 => 0,
                1..=4 => 1,
                5 => 5,
                _ => 9,
            };
            let shown = names.iter().find(|(p, _)| *p == current).map_or("None", |(_, n)| n);
            egui::ComboBox::from_id_salt(("task_priority", s.id)).width(width).selected_text(shown).show_ui(ui, |ui| {
                for (p, name) in names {
                    if ui.selectable_label(p == current, name).clicked() && p != current {
                        action = Some(TaskAction::Change(s.id, vec![Edit::priority(p)]));
                    }
                }
            });
            ui.end_row();
            ui.label("List");
            let here = lists.iter().find(|l| l.url == s.list).map_or("", |l| l.name.as_str());
            egui::ComboBox::from_id_salt(("task_list", s.id)).width(width).selected_text(here).show_ui(ui, |ui| {
                for l in lists.iter().filter(|l| l.writable) {
                    let name = format!("{}  ·  {}", l.name, l.server);
                    if ui.selectable_label(l.url == s.list, name).clicked() && l.url != s.list {
                        action = Some(TaskAction::Move(s.id, l.url.clone()));
                    }
                }
            });
            ui.end_row();
            ui.label("Labels");
            ui.scope(|ui| {
                ui.set_max_width(width);
                ui.horizontal_wrapped(|ui| {
                    let chip = |icon: Icon, text: &str| egui::Button::image_and_text(icon.image(14.0, t.text_dim), RichText::new(text).size(13.0)).corner_radius(CornerRadius::same(14)).min_size(vec2(0.0, 28.0));
                    for label in &task.categories {
                        if ui.add(chip(Icon::X, label)).on_hover_text(format!("Remove {label}")).clicked() {
                            let rest: Vec<String> = task.categories.iter().filter(|c| *c != label).cloned().collect();
                            action = Some(TaskAction::Change(s.id, vec![Edit::categories(&rest)]));
                        }
                    }
                    if !edit.labeling {
                        if ui.add(chip(Icon::Plus, "Add label")).clicked() {
                            edit.labeling = true;
                        }
                        return;
                    }
                    let id = egui::Id::new(("task_label", s.id));
                    let field = ui.add(egui::TextEdit::singleline(&mut edit.label).hint_text("Label, then Enter").desired_width(140.0).min_size(vec2(140.0, 28.0)).margin(egui::Margin::symmetric(10, 0)).vertical_align(Align::Center).id(id));
                    if !field.has_focus() && !field.lost_focus() {
                        field.request_focus();
                    }
                    if field.lost_focus() {
                        let new = edit.label.trim().trim_start_matches('#').to_owned();
                        let entered = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        edit.label.clear();
                        if entered && !new.is_empty() && !task.categories.iter().any(|c| c.eq_ignore_ascii_case(&new)) {
                            let mut all = task.categories.clone();
                            all.push(new);
                            action = Some(TaskAction::Change(s.id, vec![Edit::categories(&all)]));
                        }
                        // Enter keeps the field out for one more label; Esc or a click elsewhere puts it away.
                        edit.labeling = entered;
                    }
                });
            });
            ui.end_row();
        });
        section_title(ui, t, "Notes");
        ui.add(egui::TextEdit::multiline(&mut edit.notes).id(egui::Id::new(("task_notes", s.id))).hint_text("Add notes").desired_rows(4).desired_width(f32::INFINITY).margin(egui::Margin::same(12)));
        section_title(ui, t, "Subtasks");
        for child in children {
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
            if response.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(theme::CONTROL_RADIUS), t.hover);
            }
            let tick = Rect::from_min_size(rect.min, vec2(36.0, 36.0));
            if tick_box(ui, t, tick, child, 8.0).clicked() && child.writable {
                action = Some(TaskAction::Tick(child.id));
            }
            let title = if child.task.summary.is_empty() { "(no title)" } else { child.task.summary.as_str() };
            let color = if child.task.done { t.text_dim } else { t.text };
            let galley = views::truncated(ui, title, FontId::proportional(14.5), color, rect.width() - 48.0);
            let at = pos2(tick.right() + 4.0, rect.center().y - galley.size().y / 2.0);
            let size = galley.size();
            ui.painter().galley(at, galley, color);
            if child.task.done {
                ui.painter().line_segment([at + vec2(0.0, size.y / 2.0), at + vec2(size.x, size.y / 2.0)], egui::Stroke::new(1.0, t.text_dim));
            }
            if response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open it").clicked() {
                action = Some(TaskAction::Open(child.id));
            }
        }
        let field = ui.add(crate::sized_field(&mut edit.subtask, ui.available_width()).hint_text("Add subtask").id(egui::Id::new(("task_subtask", s.id))));
        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !edit.subtask.trim().is_empty() {
            action = Some(TaskAction::Add { title: std::mem::take(&mut edit.subtask).trim().to_owned(), list: Some(s.list.clone()), parent: Some(task.uid.clone()) });
            field.request_focus();
        }
    });
    ui.add_space(16.0);
    if task.repeats() {
        views::note(ui, Icon::Repeat, t.text_dim, "Repeats: ticking it leaves a done copy and moves it to the next date", "The repeat itself is changed in the app it was made in");
    }
    if task.alarms {
        views::note(ui, Icon::CircleAlert, t.text_dim, "Has reminders set in another app", "They are kept as they are");
    }
    if !s.writable {
        views::note(ui, Icon::Lock, t.text_dim, "Read-only list: nothing can be changed here", "");
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if let Some(id) = task.message_id()
            && ui.add(egui::Button::image_and_text(Icon::Mail.image(icons::SIZE, t.text), "Open message")).on_hover_text("The message this task was made from").clicked()
        {
            action = Some(TaskAction::OpenMessage(id.to_owned()));
        }
        if ui.add_enabled(s.writable, egui::Button::image_and_text(Icon::Trash.image(icons::SIZE, t.danger), RichText::new("Delete").color(t.danger))).clicked() {
            action = Some(TaskAction::Delete(s.id));
        }
    });
    action
}

/// A quick day in the due picker: its icon and name, and the day it falls on, dim, at the right.
fn quick_row(ui: &mut egui::Ui, t: &Tokens, icon: Icon, label: &str, hint: &str, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(width, 34.0), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), t.hover);
    }
    icon.image(16.0, t.text_dim).paint_at(ui, Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(16.0, 16.0)));
    ui.painter().text(pos2(rect.left() + 36.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.5), t.text);
    ui.painter().text(pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, hint, FontId::proportional(13.0), t.text_dim);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The title of a part of the details below the grid (Notes, Subtasks).
fn section_title(ui: &mut egui::Ui, t: &Tokens, title: &str) {
    ui.add_space(20.0);
    ui.label(RichText::new(title).family(theme::bold()).color(t.text));
    ui.add_space(6.0);
}

/// When a task is due, in one row of `width`: a button with the day (its picker holds the quick days
/// and No date), and the time once there is a day. Returns the change made.
fn due_row(ui: &mut egui::Ui, t: &Tokens, task: &Task, month: &mut NaiveDate, width: f32) -> Option<Vec<Edit>> {
    let mut out = None;
    let today = Local::now().date_naive();
    let current = due(task);
    let set = |day: Option<NaiveDate>, time: Option<NaiveTime>| Some(vec![Edit::due(task, day.map(|d| due_when(d, time)).as_ref())]);
    ui.horizontal(|ui| {
        let time_w = 110.0;
        let day_w = if current.is_some() { width - time_w - ui.spacing().item_spacing.x } else { width };
        let (rect, button) = ui.allocate_exact_size(vec2(day_w, theme::CONTROL_H), Sense::click());
        let look = ui.style().interact(&button);
        ui.painter().rect(rect, look.corner_radius, look.weak_bg_fill, look.bg_stroke, egui::StrokeKind::Inside);
        let late = current.is_some() && !task.done && group(task, Local::now().naive_local()) == Group::Overdue;
        let (text, color) = match current {
            // "Tomorrow  ·  2 Dec" close by, the full date further; the time has its own field.
            Some((d, _)) => {
                let near = match (d - today).num_days() {
                    0 => Some("Today".to_owned()),
                    1 => Some("Tomorrow".to_owned()),
                    -1 => Some("Yesterday".to_owned()),
                    2..=6 => Some(d.format("%A").to_string()),
                    _ => None,
                };
                let text = near.map_or_else(|| d.format("%a %-d %b %Y").to_string(), |n| format!("{n}  ·  {}", d.format("%-d %b")));
                (text, if late { t.danger } else { t.text })
            }
            None => ("No date".to_owned(), t.text_dim),
        };
        Icon::Calendar.image(icons::SIZE, color).paint_at(ui, Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(icons::SIZE, icons::SIZE)));
        let galley = views::truncated(ui, &text, FontId::proportional(15.0), color, rect.width() - 52.0);
        ui.painter().galley(pos2(rect.left() + 40.0, rect.center().y - galley.size().y / 2.0), galley, color);
        let button = button.on_hover_cursor(egui::CursorIcon::PointingHand);
        // As wide as the month, under the field's left edge: the quick days are rows, not a wider strip.
        let width = 7.0 * 34.0;
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .align(egui::RectAlign::BOTTOM_START)
            .width(width)
            .show(|ui| {
                let next_monday = today + Duration::days(i64::from(7 - today.weekday().num_days_from_monday()));
                let quick = [(Icon::Sun, "Today", Some(today)), (Icon::Sunrise, "Tomorrow", Some(today + Duration::days(1))), (Icon::CalendarRange, "Next week", Some(next_monday)), (Icon::CalendarX, "No date", None)];
                for (icon, label, day) in quick {
                    if day.is_none() && current.is_none() {
                        continue;
                    }
                    let hint = day.map_or_else(String::new, |d| if d == next_monday { d.format("%a %-d %b").to_string() } else { d.format("%a").to_string() });
                    if quick_row(ui, t, icon, label, &hint, width).clicked() {
                        out = set(day, day.and(current.and_then(|(_, time)| time)));
                        ui.close();
                    }
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                let mut day = current.map_or(today, |(d, _)| d);
                let before = day;
                views::calendar(ui, t, &mut day, month, today, None);
                if day != before {
                    out = set(Some(day), current.and_then(|(_, time)| time));
                    ui.close();
                }
            });
        if let Some((day, time)) = current {
            let minutes = time.map(|t| t.num_seconds_from_midnight() / 60);
            let shown = time.map_or_else(|| "No time".to_owned(), |t| t.format("%H:%M").to_string());
            egui::ComboBox::from_id_salt("task_due_time").width(time_w).height(320.0).selected_text(shown).show_ui(ui, |ui| {
                if ui.selectable_label(minutes.is_none(), "No time").clicked() && minutes.is_some() {
                    out = set(Some(day), None);
                }
                for m in (0..24 * 60).step_by(15) {
                    if ui.selectable_label(minutes == Some(m), format!("{:02}:{:02}", m / 60, m % 60)).clicked() {
                        out = set(Some(day), NaiveTime::from_num_seconds_from_midnight_opt(m * 60, 0));
                    }
                }
            });
        }
    });
    out
}

/// The dialog to make a task of a message: filled from it, saved only when asked.
#[derive(Clone, Debug, Default)]
pub struct FromMessage {
    pub title: String,
    pub notes: String,
    /// The list it goes to (by URL).
    pub list: String,
    /// Without `<>`.
    pub message_id: String,
    pub focus: bool,
}

impl FromMessage {
    /// From a message's subject and the start of its text (quoted lines and the signature left out).
    pub fn new(subject: &str, text: &str, message_id: &str, list: &str) -> Self {
        let mut lines: Vec<&str> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end();
            if line == "--" || line == "-- " || line.starts_with("On ") && line.ends_with("wrote:") {
                break;
            }
            if line.starts_with('>') || (line.is_empty() && lines.last().is_none_or(|l| l.is_empty())) {
                continue;
            }
            lines.push(line);
            if lines.len() >= 8 {
                break;
            }
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let title = subject.trim();
        let title = ["Re:", "RE:", "Fwd:", "FW:", "Fw:", "R:", "I:"].iter().fold(title, |t, p| t.strip_prefix(p).map_or(t, str::trim_start));
        Self { title: title.to_owned(), notes: lines.join("\n"), list: list.to_owned(), message_id: message_id.to_owned(), focus: true }
    }
}

/// Returns the task to make when Make task is pressed.
pub fn from_message_dialog(ctx: &egui::Context, t: &Tokens, state: &mut Option<FromMessage>, lists: &[List]) -> Option<FromMessage> {
    let form = state.as_mut()?;
    let (mut made, mut close) = (false, false);
    let modal = views::modal(ctx, egui::Id::new("task_from_message")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        ui.heading("Make a task");
        ui.label(RichText::new("It links back to the message, to open it again from Tasks.").color(t.text_dim));
        ui.add_space(12.0);
        crate::form_grid(ui, "task_from_grid", |ui| {
            ui.label("Title");
            let title = ui.add(crate::field(&mut form.title).hint_text("(no title)"));
            if std::mem::take(&mut form.focus) {
                title.request_focus();
            }
            ui.end_row();
            ui.label("List");
            let shown = lists.iter().find(|l| l.url == form.list).map_or("", |l| l.name.as_str()).to_owned();
            egui::ComboBox::from_id_salt("task_from_list").width(crate::FIELD_WIDTH).selected_text(shown).show_ui(ui, |ui| {
                for l in lists.iter().filter(|l| l.writable) {
                    ui.selectable_value(&mut form.list, l.url.clone(), format!("{}  ·  {}", l.name, l.server));
                }
            });
            ui.end_row();
        });
        ui.add_space(12.0);
        ui.label(RichText::new("Notes").color(t.text_dim));
        ui.add(egui::TextEdit::multiline(&mut form.notes).id_salt("task_from_notes").desired_rows(4).desired_width(f32::INFINITY).margin(egui::Margin::same(12)));
        ui.add_space(16.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let can = !form.title.trim().is_empty() && !form.list.is_empty();
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && !ui.ctx().memory(|m| m.has_focus(egui::Id::new("task_from_notes")));
            let why = if form.list.is_empty() { "Pick a list" } else { "Give it a title" };
            if views::primary_button(ui, t, "Make task", can).on_disabled_hover_text(why).clicked() || (can && enter) {
                made = true;
            }
            close = ui.button("Cancel").clicked();
        });
    });
    if made {
        return state.take();
    }
    if close || modal.should_close() {
        *state = None;
    }
    None
}

/// The dialog to make a task list on a server.
#[derive(Clone, Debug, Default)]
pub struct NewList {
    pub name: String,
    /// The server, as one of its calendars: (URL, user).
    pub server: usize,
    pub making: bool,
    pub error: Option<String>,
}

/// Returns the server chosen (its place in `servers`) and the name, when Create list is pressed.
pub fn new_list_dialog(ctx: &egui::Context, t: &Tokens, state: &mut Option<NewList>, servers: &[String]) -> Option<(usize, String)> {
    let form = state.as_mut()?;
    let (mut made, mut close) = (None, false);
    let modal = views::modal(ctx, egui::Id::new("new_task_list")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        ui.heading("New task list");
        ui.label(RichText::new("Made on the server, so your other devices see it too.").color(t.text_dim));
        ui.add_space(12.0);
        crate::form_grid(ui, "new_list_grid", |ui| {
            ui.label("Name");
            let field = ui.add(crate::field(&mut form.name).hint_text("Tasks"));
            if ui.memory(|m| m.focused().is_none()) {
                field.request_focus();
            }
            ui.end_row();
            ui.label("Server");
            let shown = servers.get(form.server).cloned().unwrap_or_default();
            egui::ComboBox::from_id_salt("new_list_server").width(crate::FIELD_WIDTH).selected_text(shown).show_ui(ui, |ui| {
                for (i, s) in servers.iter().enumerate() {
                    ui.selectable_value(&mut form.server, i, s);
                }
            });
            ui.end_row();
        });
        if let Some(error) = &form.error {
            ui.add_space(8.0);
            views::note(ui, Icon::Ban, t.danger, error, "");
        }
        ui.add_space(16.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let can = !form.making && !servers.is_empty();
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if views::primary_button(ui, t, "Create list", can).on_disabled_hover_text(if servers.is_empty() { "Add a calendar server first" } else { "Making it…" }).clicked() || (can && enter) {
                let name = if form.name.trim().is_empty() { "Tasks".to_owned() } else { form.name.trim().to_owned() };
                made = Some((form.server, name));
            }
            close = ui.button("Cancel").clicked();
            if form.making {
                ui.spinner();
            }
        });
    });
    if close || (modal.should_close() && !form.making) {
        *state = None;
    }
    made
}

/// The rail's count of open tasks due today or before, as a small badge on its icon.
pub fn badge(ui: &egui::Ui, t: &Tokens, button: Rect, count: u32) {
    if count == 0 {
        return;
    }
    let text = if count > 99 { "99+".to_owned() } else { count.to_string() };
    let galley = ui.painter().layout_no_wrap(text, FontId::proportional(11.0), t.on_accent);
    let size = vec2((galley.size().x + 8.0).max(16.0), 16.0);
    let pill = Rect::from_center_size(pos2(button.right() - size.x / 2.0 + 2.0, button.top() + 6.0), size);
    ui.painter().rect_filled(pill, CornerRadius::same(8), t.danger);
    ui.painter().text(pill.center(), Align2::CENTER_CENTER, galley.text(), FontId::proportional(11.0), t.on_accent);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beavermail_carddav::todo;

    fn task(id: i64, uid: &str, due: Option<&str>, parent: Option<&str>, done: bool) -> Shown {
        let mut task = Task { uid: uid.into(), summary: uid.into(), parent: parent.map(str::to_owned), done, ..Task::default() };
        task.due = due.map(|d| When { value: d.into(), tzid: None, date_only: true });
        Shown { id, list: "L".into(), task, writable: true }
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 25).unwrap().and_hms_opt(12, 0, 0).unwrap()
    }

    fn uids(shown: &[Shown], rows: &[(Group, Vec<Line>)]) -> Vec<(Group, Vec<(String, usize)>)> {
        rows.iter().map(|(g, r)| (*g, r.iter().map(|l| (shown[l.at].task.uid.clone(), l.depth)).collect())).collect()
    }

    #[test]
    fn open_tasks_are_grouped_by_when_they_are_due_with_subtasks_under_their_parent() {
        let shown = vec![
            task(1, "late", Some("20261120"), None, false),
            task(2, "today", Some("20261125"), None, false),
            task(3, "later", Some("20261210"), None, false),
            task(4, "someday", None, None, false),
            task(5, "part", Some("20261101"), Some("someday"), false),
            task(6, "done", Some("20261125"), None, true),
        ];
        let rows = lines(&shown, &Filter::All, now(), &HashSet::new(), &HashSet::new());
        let s = |u: &str, d| (u.to_owned(), d);
        assert_eq!(
            uids(&shown, &rows),
            [(Group::Overdue, vec![s("late", 0)]), (Group::Today, vec![s("today", 0)]), (Group::Upcoming, vec![s("later", 0)]), (Group::NoDate, vec![s("someday", 0), s("part", 1)])],
            "a subtask goes with its parent, not by its own date; done ones are left out"
        );
        let today = lines(&shown, &Filter::Today, now(), &HashSet::from([6]), &HashSet::new());
        assert_eq!(uids(&shown, &today), [(Group::Overdue, vec![s("part", 0), s("late", 0)]), (Group::Today, vec![s("today", 0), s("done", 0)])], "a subtask without its parent stands alone; one just ticked stays");
        let folded = lines(&shown, &Filter::All, now(), &HashSet::new(), &HashSet::from(["someday".to_owned()]));
        assert_eq!(folded.last().unwrap().1.len(), 1);
    }

    #[test]
    fn the_order_put_by_hand_counts_among_tasks_due_alike_only() {
        let mut shown = vec![task(1, "a", Some("20261126"), None, false), task(2, "b", Some("20261126"), None, false), task(3, "c", Some("20261127"), None, false), task(4, "d", None, None, false), task(5, "e", None, None, false)];
        shown[1].task.sort = Some(1024);
        shown[0].task.sort = Some(2048);
        shown[2].task.sort = Some(1);
        shown[4].task.sort = Some(10);
        let rows = lines(&shown, &Filter::All, now(), &HashSet::new(), &HashSet::new());
        let s = |u: &str| (u.to_owned(), 0);
        assert_eq!(uids(&shown, &rows), [(Group::Upcoming, vec![s("b"), s("a"), s("c")]), (Group::NoDate, vec![s("e"), s("d")])], "the day comes first, then the hand's order; one without goes last");
        assert!(same_slot(&shown[0], &shown[1]) && !same_slot(&shown[1], &shown[2]) && same_slot(&shown[3], &shown[4]));
    }

    #[test]
    fn a_loop_of_parents_is_shown_once_and_never_hangs() {
        let shown = vec![task(1, "a", None, Some("b"), false), task(2, "b", None, Some("a"), false), task(3, "c", None, Some("c"), false)];
        let rows = lines(&shown, &Filter::All, now(), &HashSet::new(), &HashSet::new());
        let all: Vec<String> = rows.iter().flat_map(|(_, r)| r.iter().map(|l| shown[l.at].task.uid.clone())).collect();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn a_task_from_a_message_takes_its_subject_and_the_start_of_its_text() {
        let text = "Hi,\n\n\nCan you send the figures by Friday?\n\nThanks\n-- \nBob\n";
        let f = FromMessage::new("Re: Q4 figures", text, "abc@example.org", "L");
        assert_eq!(f.title, "Q4 figures");
        assert_eq!(f.notes, "Hi,\n\nCan you send the figures by Friday?\n\nThanks");
        let quoted = FromMessage::new("Plan", "Sounds good.\n\nOn Mon, Bob wrote:\n> the plan\n", "", "L");
        assert_eq!(quoted.notes, "Sounds good.");
    }

    #[test]
    fn a_due_time_is_kept_in_utc_and_read_back_here() {
        let day = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let time = NaiveTime::from_hms_opt(15, 0, 0);
        let w = due_when(day, time);
        assert!(w.value.ends_with('Z') && !w.date_only);
        let text = todo::new("x", &[Edit::due(&Task::default(), Some(&w))], Utc::now());
        assert_eq!(due(&todo::parse(&text).unwrap()), Some((day, time)));
        assert_eq!(due_when(day, None).value, "20261201");
    }
}
