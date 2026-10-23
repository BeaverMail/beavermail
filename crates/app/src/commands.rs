// SPDX-License-Identifier: AGPL-3.0-only

//! Every user action that has a key: its name, the shortcut shown for it and the key that runs it.
//! The keys, the command palette and the tooltips read them from here, so each action has one
//! name and one shortcut everywhere.

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Palette,
    NewMessage,
    Reply,
    ReplyAll,
    Forward,
    Archive,
    Trash,
    Undo,
    ToggleRead,
    Pin,
    Star,
    /// One of the five standard labels, `$label1` to `$label5`, on or off, by its number.
    Label(u8),
    Spam,
    TrustSender,
    BlockSender,
    ViewSource,
    FindInSource,
    OpenInTab,
    CloseTab,
    PreviousTab,
    NextTab,
    SelectAll,
    Search,
    Refresh,
    SwitchPane,
    AllInboxes,
    Mail,
    Calendar,
    Contacts,
    WorkOffline,
    WorkOnline,
    Settings,
}

/// What runs a command.
enum Trigger {
    /// A key without Ctrl or Alt, while no text field has the keyboard.
    Key(Key),
    /// A typed character, whatever key the layout puts it on ("!", "/" is Shift+7 on some).
    Text(&'static str),
    /// A key with modifiers; `typing`: it works in a text field too.
    Shortcut(Modifiers, Key, bool),
}

const CTRL: Modifiers = Modifiers::COMMAND;
const CTRL_SHIFT: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);

impl Command {
    /// In the order the keys are tried (Ctrl+Shift+Tab before Ctrl+Tab) and the palette lists them.
    pub const ALL: [Self; 36] = [
        Self::Palette,
        Self::NewMessage,
        Self::Reply,
        Self::ReplyAll,
        Self::Forward,
        Self::Archive,
        Self::Trash,
        Self::Undo,
        Self::ToggleRead,
        Self::Pin,
        Self::Star,
        Self::Label(1),
        Self::Label(2),
        Self::Label(3),
        Self::Label(4),
        Self::Label(5),
        Self::Spam,
        Self::TrustSender,
        Self::BlockSender,
        Self::ViewSource,
        Self::FindInSource,
        Self::OpenInTab,
        Self::CloseTab,
        Self::PreviousTab,
        Self::NextTab,
        Self::SelectAll,
        Self::Search,
        Self::Refresh,
        Self::SwitchPane,
        Self::AllInboxes,
        Self::Mail,
        Self::Calendar,
        Self::Contacts,
        Self::WorkOffline,
        Self::WorkOnline,
        Self::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Palette => "Show all commands",
            Self::NewMessage => "New message",
            Self::Reply => "Reply",
            Self::ReplyAll => "Reply all",
            Self::Forward => "Forward",
            Self::Archive => "Archive",
            Self::Trash => "Move to Trash",
            Self::Undo => "Undo archive or move to Trash",
            Self::ToggleRead => "Mark as read or unread",
            Self::Pin => "Pin or unpin",
            Self::Star => "Star or unstar",
            Self::Label(1) => "Label Important",
            Self::Label(2) => "Label Work",
            Self::Label(3) => "Label Personal",
            Self::Label(4) => "Label To Do",
            Self::Label(_) => "Label Later",
            Self::Spam => "Spam",
            Self::TrustSender => "Trust sender",
            Self::BlockSender => "Block sender",
            Self::ViewSource => "View source",
            Self::FindInSource => "Find in source",
            Self::OpenInTab => "Open in a tab",
            Self::CloseTab => "Close tab",
            Self::PreviousTab => "Previous tab",
            Self::NextTab => "Next tab",
            Self::SelectAll => "Select all messages",
            Self::Search => "Search mail",
            Self::Refresh => "Refresh",
            Self::SwitchPane => "Switch between list and reader",
            Self::AllInboxes => "Go to All inboxes",
            Self::Mail => "Go to Mail",
            Self::Calendar => "Go to Calendar",
            Self::Contacts => "Go to Contacts",
            Self::WorkOffline => "Work offline",
            Self::WorkOnline => "Work online",
            Self::Settings => "Settings",
        }
    }

    /// Its keys, the one to show first; none for a command run only from the palette.
    fn triggers(self) -> &'static [Trigger] {
        match self {
            Self::Palette => &[Trigger::Shortcut(CTRL, Key::K, true)],
            Self::NewMessage => &[Trigger::Shortcut(CTRL, Key::N, true), Trigger::Key(Key::C)],
            Self::Reply => &[Trigger::Key(Key::R)],
            Self::ReplyAll => &[Trigger::Key(Key::A)],
            Self::Forward => &[Trigger::Key(Key::F)],
            Self::Archive => &[Trigger::Key(Key::E)],
            Self::Trash => &[Trigger::Key(Key::Delete)],
            Self::Undo => &[Trigger::Shortcut(CTRL, Key::Z, false)],
            Self::ToggleRead => &[Trigger::Key(Key::U)],
            Self::Pin => &[Trigger::Key(Key::P)],
            Self::Star => &[Trigger::Key(Key::S)],
            Self::Label(1) => &[Trigger::Text("1")],
            Self::Label(2) => &[Trigger::Text("2")],
            Self::Label(3) => &[Trigger::Text("3")],
            Self::Label(4) => &[Trigger::Text("4")],
            Self::Label(_) => &[Trigger::Text("5")],
            Self::Spam => &[Trigger::Text("!")],
            Self::ViewSource => &[Trigger::Shortcut(CTRL, Key::U, false)],
            Self::FindInSource => &[Trigger::Shortcut(CTRL, Key::F, true)],
            Self::OpenInTab => &[Trigger::Key(Key::Enter)],
            Self::CloseTab => &[Trigger::Shortcut(CTRL, Key::W, false)],
            Self::PreviousTab => &[Trigger::Shortcut(CTRL_SHIFT, Key::Tab, false)],
            Self::NextTab => &[Trigger::Shortcut(CTRL, Key::Tab, false)],
            Self::SelectAll => &[Trigger::Shortcut(CTRL, Key::A, false)],
            Self::Search => &[Trigger::Text("/")],
            Self::SwitchPane => &[Trigger::Key(Key::F6)],
            Self::TrustSender | Self::BlockSender | Self::Refresh | Self::AllInboxes | Self::Mail | Self::Calendar | Self::Contacts | Self::WorkOffline | Self::WorkOnline | Self::Settings => &[],
        }
    }

    /// The shortcut as the user reads it ("R", "Ctrl+N or C", "⌘N" on macOS); empty when there is none.
    pub fn keys(self, ctx: &egui::Context) -> String {
        let one = |t: &Trigger| match t {
            Trigger::Key(key) => key.symbol_or_name().to_owned(),
            Trigger::Text(text) => (*text).to_owned(),
            Trigger::Shortcut(modifiers, key, _) => ctx.format_shortcut(&KeyboardShortcut::new(*modifiers, *key)),
        };
        self.triggers().iter().map(one).collect::<Vec<_>>().join(" or ")
    }

    /// "Reply (R)", for a tooltip.
    pub fn tip(self, ctx: &egui::Context) -> String {
        match self.keys(ctx) {
            keys if keys.is_empty() => self.label().to_owned(),
            keys => format!("{} ({keys})", self.label()),
        }
    }

    /// Whether its key was pressed this frame; a shortcut is consumed. `typing`: a text field has the
    /// keyboard, so only shortcuts meant to work there count.
    pub fn pressed(self, ctx: &egui::Context, typing: bool) -> bool {
        self.triggers().iter().any(|t| match t {
            Trigger::Shortcut(modifiers, key, anywhere) => (*anywhere || !typing) && ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(*modifiers, *key))),
            Trigger::Key(key) => !typing && ctx.input(|i| i.key_pressed(*key) && !i.modifiers.command && !i.modifiers.alt),
            Trigger::Text(text) => !typing && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == text))),
        })
    }

    /// Whether `query` (words, any order, any case) finds this command or `name`.
    pub fn matches(name: &str, query: &str) -> bool {
        let name = name.to_lowercase();
        query.split_whitespace().all(|word| name.contains(&word.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_is_listed_once_with_its_own_name_and_key() {
        let ctx = egui::Context::default();
        // One frame loads the fonts: on macOS the keys are written with symbols (⌘) when a font has them.
        ctx.run_ui(Default::default(), |_| {}).textures_delta.clear();
        for (i, a) in Command::ALL.iter().enumerate() {
            for b in &Command::ALL[i + 1..] {
                assert_ne!(a, b, "listed twice");
                assert_ne!(a.label(), b.label(), "one name, one action");
                let (ka, kb) = (a.keys(&ctx), b.keys(&ctx));
                assert!(ka.is_empty() || ka != kb, "{a:?} and {b:?} share {ka}");
            }
        }
        assert_eq!(Command::Reply.tip(&ctx), "Reply (R)");
        let ctrl_n = ctx.format_shortcut(&KeyboardShortcut::new(CTRL, egui::Key::N));
        assert_eq!(Command::NewMessage.keys(&ctx), format!("{ctrl_n} or C"));
    }

    #[test]
    fn the_palette_finds_words_in_any_order() {
        assert!(Command::matches("Move to Trash", "trash mo"));
        assert!(Command::matches("Go to Work › Inbox", "work inbox"));
        assert!(!Command::matches("Reply all", "forward"));
    }
}
