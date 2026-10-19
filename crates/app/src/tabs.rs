// SPDX-License-Identifier: AGPL-3.0-only

//! Messages open in tabs across the top of the window. The first tab is
//! the mail itself (folders, list and reading pane); a double click, a middle click, Ctrl+click or
//! Enter opens a message in a tab of its own, as wide as the window. A plain click only shows it in
//! the reading pane: that one is kept here too, unpinned, so going back to the mail shows it again.
//! A tab remembers where its message came from, so it stays usable after another folder or account
//! is shown.

use beavermail_core::{MessageId, MessageSummary, Role};
use eframe::egui::{self, CornerRadius, FontId, Rect, Sense, Ui, pos2, vec2};

use crate::icons::Icon;
use crate::theme::Tokens;

pub struct Tab {
    /// The account the message belongs to.
    pub serial: u64,
    pub message: MessageSummary,
    /// What the folder it was opened from is for: deleting from the Trash erases, and so on.
    pub role: Role,
    /// A tab of its own; otherwise the message in the reading pane, replaced by the next one shown.
    pub pinned: bool,
    /// Shows the message's source (View source) instead of the message; always a tab of its own.
    pub source: bool,
}

/// Where a message opens.
#[derive(Debug, PartialEq, Eq)]
pub enum Slot {
    /// It already has this tab.
    Existing(usize),
    /// It takes the preview tab's place.
    Replace(usize),
    /// It gets a new tab at the end.
    Push,
}

/// Where message `id` opens: `pin`, in its tab, or a new one; else in the reading pane, replacing
/// the message there. A message can be in both, never in two tabs.
pub fn slot(tabs: &[Tab], id: MessageId, pin: bool) -> Slot {
    match tabs.iter().position(|t| t.message.id == id && t.pinned == pin && !t.source) {
        Some(at) => Slot::Existing(at),
        None => match tabs.iter().position(|t| !t.pinned && !t.source).filter(|_| !pin) {
            Some(preview) => Slot::Replace(preview),
            None => Slot::Push,
        },
    }
}

/// What the user did on the tab bar.
pub enum TabEvent {
    /// Back to the mail.
    Mail,
    Activate(usize),
    Close(usize),
}

const TAB_H: f32 = 34.0;
const TAB_MAX_W: f32 = 200.0;

/// The mail, then each message tab; `active` is the message tab shown, `None` the mail.
pub fn tab_bar(ui: &mut Ui, t: &Tokens, tabs: &[Tab], active: Option<usize>) -> Option<TabEvent> {
    let mut event = None;
    egui::ScrollArea::horizontal().id_salt("tabs").auto_shrink([false, true]).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (_, mail) = tab(ui, t, Some(Icon::Mail), "Mail", active.is_none(), false);
            if mail.on_hover_text("Folders and messages (Ctrl+Tab)").clicked() {
                event = Some(TabEvent::Mail);
            }
            for (i, tab_) in tabs.iter().enumerate().filter(|(_, tab)| tab.pinned) {
                let title = if tab_.message.subject.trim().is_empty() { "(no subject)" } else { tab_.message.subject.as_str() };
                let (title, icon) = if tab_.source { (format!("Source: {title}"), Some(Icon::FileCode)) } else { (title.to_owned(), None) };
                let (close, response) = tab(ui, t, icon, &title, active == Some(i), true);
                let response = response.on_hover_text(format!("{}\n{}", tab_.message.from, tab_.message.subject));
                if close.is_some_and(|c| c.on_hover_text("Close (Ctrl+W)").clicked()) || response.middle_clicked() {
                    event = Some(TabEvent::Close(i));
                } else if response.clicked() {
                    event = Some(TabEvent::Activate(i));
                }
            }
        });
    });
    event
}

/// One tab: its close button, if `closable`, and the tab itself.
fn tab(ui: &mut Ui, t: &Tokens, icon: Option<Icon>, title: &str, current: bool, closable: bool) -> (Option<egui::Response>, egui::Response) {
    let font = FontId::proportional(13.5);
    let color = if current { t.text } else { t.text_dim };
    let lead = if icon.is_some() { 32.0 } else { 12.0 };
    let trail = if closable { 32.0 } else { 14.0 };
    let mut job = egui::text::LayoutJob::simple_singleline(title.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(TAB_MAX_W - lead - trail);
    let galley = ui.painter().layout_job(job);
    let (rect, response) = ui.allocate_exact_size(vec2(galley.size().x + lead + trail, TAB_H), Sense::click());
    let fill = if current {
        Some(t.surface)
    } else if response.hovered() {
        Some(t.hover)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    }
    if current {
        ui.painter().rect_stroke(rect, CornerRadius::same(6), egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        ui.painter().line_segment([pos2(rect.left() + 8.0, rect.bottom() - 1.0), pos2(rect.right() - 8.0, rect.bottom() - 1.0)], egui::Stroke::new(2.0, t.accent));
    }
    if let Some(icon) = icon {
        icon.image(16.0, color).paint_at(ui, Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(16.0, 16.0)));
    }
    ui.painter().galley(pos2(rect.left() + lead, rect.center().y - galley.size().y / 2.0), galley, color);
    let close = closable.then(|| {
        let close = Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(20.0, 20.0));
        let close = ui.interact(close, response.id.with("close"), Sense::click());
        if current || response.hovered() || close.hovered() {
            let tint = if close.hovered() { t.text } else { t.text_dim };
            Icon::X.image(13.0, tint).paint_at(ui, Rect::from_center_size(close.rect.center(), vec2(13.0, 13.0)));
        }
        close
    });
    (close, response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u64, pinned: bool) -> Tab {
        Tab { serial: 1, message: MessageSummary { id: MessageId(id), ..sample() }, role: Role::Inbox, pinned, source: false }
    }

    fn sample() -> MessageSummary {
        MessageSummary {
            id: MessageId(0),
            from: String::new(),
            address: String::new(),
            subject: String::new(),
            preview: String::new(),
            received: String::new(),
            timestamp: 0,
            unread: false,
            pinned: false,
            starred: false,
            priority: 0,
            labels: Vec::new(),
            has_attachments: false,
            message_id: String::new(),
            parents: Vec::new(),
        }
    }

    #[test]
    fn a_click_shows_in_the_pane_and_a_tab_is_opened_once() {
        assert_eq!(slot(&[], MessageId(1), false), Slot::Push, "the first message gets a tab");
        let tabs = [tab(1, true), tab(2, false)];
        assert_eq!(slot(&tabs, MessageId(3), false), Slot::Replace(1), "a click takes the preview tab");
        assert_eq!(slot(&tabs, MessageId(3), true), Slot::Push, "kept: a tab of its own");
        assert_eq!(slot(&tabs, MessageId(1), true), Slot::Existing(0), "a message is not in two tabs");
        assert_eq!(slot(&tabs, MessageId(2), true), Slot::Push, "the reading pane's message gets a tab too");
        assert_eq!(slot(&tabs, MessageId(1), false), Slot::Replace(1), "and a tab's message can be shown in the pane");
        assert_eq!(slot(&[tab(1, true)], MessageId(2), false), Slot::Push, "nothing in the pane yet");
    }

    #[test]
    fn a_source_tab_is_never_the_message_or_the_pane() {
        let source = Tab { source: true, ..tab(1, true) };
        assert_eq!(slot(&[source], MessageId(1), true), Slot::Push, "the message gets its own tab beside its source");
        let source = Tab { source: true, ..tab(1, true) };
        assert_eq!(slot(&[source], MessageId(2), false), Slot::Push, "nor does the pane take the source's place");
    }
}
