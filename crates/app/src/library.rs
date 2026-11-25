// SPDX-License-Identifier: AGPL-3.0-only

//! The Attachments section: the files of every downloaded message, found by name, sender or kind,
//! as a list or a grid of pictures, opened, saved, or followed back to their message.

use std::collections::HashMap;

use beavermail_core::{Attachment, MessageId};
use beavermail_store::LibraryFile;
use eframe::egui::{self, CornerRadius, FontId, Key as KeyCode, Layout, Rect, RichText, Sense, Ui, pos2, vec2};

use crate::icons::{self, Icon};
use crate::theme::{self, CONTROL_RADIUS, Tokens};
use crate::views;

/// A file of the library: its message, and its place among the message's attachments.
pub type Key = (MessageId, u32);

pub fn key(file: &LibraryFile) -> Key {
    (file.message, file.file.index)
}

/// Pictures drawn at most this wide or tall, for the grid and the preview alike.
pub const THUMB_SIDE: u32 = 480;
/// Larger pictures are not decoded for a preview: an icon stands for them.
const PREVIEW_MAX_BYTES: u64 = 30 * 1024 * 1024;
const ROW_H: f32 = 56.0;
const LEAD: f32 = 36.0;
const TILE_W: f32 = 164.0;
const TILE_H: f32 = 176.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    #[default]
    All,
    Images,
    Pdf,
    Documents,
    Other,
}

/// Text, spreadsheets and slides, by the extension their programs give them.
const DOCUMENTS: [&str; 17] = ["doc", "docx", "odt", "rtf", "txt", "md", "pages", "xls", "xlsx", "ods", "csv", "numbers", "ppt", "pptx", "odp", "key", "epub"];

impl Kind {
    const ALL: [Kind; 5] = [Kind::All, Kind::Images, Kind::Pdf, Kind::Documents, Kind::Other];

    pub fn of(file: &Attachment) -> Self {
        let mime = file.mime.to_ascii_lowercase();
        let extension = extension(&file.name);
        if mime.starts_with("image/") {
            Self::Images
        } else if mime == "application/pdf" || extension == "pdf" {
            Self::Pdf
        } else if mime.starts_with("text/") || DOCUMENTS.contains(&extension.as_str()) {
            Self::Documents
        } else {
            Self::Other
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::All => "All files",
            Self::Images => "Pictures",
            Self::Pdf => "PDFs",
            Self::Documents => "Documents",
            Self::Other => "Other files",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::All => Icon::Paperclip,
            Self::Images => Icon::Image,
            Self::Pdf | Self::Documents => Icon::FileText,
            Self::Other => Icon::File,
        }
    }
}

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Whether a picture of this file can be drawn: a JPEG or PNG of a sensible size.
pub fn previewable(file: &Attachment) -> bool {
    let mime = file.mime.to_ascii_lowercase();
    let picture = matches!(mime.as_str(), "image/jpeg" | "image/jpg" | "image/png") || matches!(extension(&file.name).as_str(), "jpg" | "jpeg" | "png");
    picture && file.size <= PREVIEW_MAX_BYTES
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Newest,
    Largest,
    Name,
}

impl Sort {
    fn title(self) -> &'static str {
        match self {
            Self::Newest => "Newest first",
            Self::Largest => "Largest first",
            Self::Name => "By name",
        }
    }
}

/// A picture of a file: being read, ready, or none to be had (not a picture the app reads).
pub enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    Missing,
}

/// What the section shows and keeps between frames.
#[derive(Default)]
pub struct Library {
    /// Read from the archive when the section opens; `None` until then.
    pub files: Option<Vec<LibraryFile>>,
    pub query: String,
    pub kind: Kind,
    /// The account shown, by its key; `None` for every one.
    pub account: Option<String>,
    pub grid: bool,
    pub sort: Sort,
    pub picked: Option<Key>,
    pub thumbs: HashMap<Key, Thumb>,
    /// When the files were last read from the archive, and whether mail was being downloaded or read then.
    pub read_at: Option<std::time::Instant>,
    pub working: bool,
}

impl Library {
    pub fn file(&self, key: Key) -> Option<&LibraryFile> {
        self.files.as_ref()?.iter().find(|f| self::key(f) == key)
    }

    #[cfg(test)]
    fn shown(&self) -> Vec<&LibraryFile> {
        shown(self.files.as_deref().unwrap_or_default(), self)
    }
}

/// The files shown: of the kind and account picked, matching every word typed, in the order chosen.
fn shown<'a>(files: &'a [LibraryFile], library: &Library) -> Vec<&'a LibraryFile> {
    let words: Vec<String> = library.query.split_whitespace().map(str::to_lowercase).collect();
    let mut shown: Vec<&LibraryFile> = files
        .iter()
        .filter(|f| library.account.as_ref().is_none_or(|a| *a == f.account))
        .filter(|f| library.kind == Kind::All || Kind::of(&f.file) == library.kind)
        .filter(|f| {
            let text = format!("{} {} {}", f.file.name, f.from, f.subject).to_lowercase();
            words.iter().all(|w| text.contains(w))
        })
        .collect();
    match library.sort {
        Sort::Newest => {}
        Sort::Largest => shown.sort_by_key(|f| std::cmp::Reverse(f.file.size)),
        Sort::Name => shown.sort_by_cached_key(|f| f.file.name.to_lowercase()),
    }
    shown
}

/// What the user asked of a file.
pub enum Action {
    Open(Key),
    Save(Key),
    GoTo(Key),
    /// Its picture is wanted on screen and not read yet.
    Load(Key),
}

/// The section: kinds and accounts on the left, the files in the middle, the one picked on the right.
/// `accounts`: (key, address) of each account, the folders on this computer included. `progress`:
/// (done, total) messages while mail is being downloaded or read, so more files may come.
pub fn section(ui: &mut Ui, t: &Tokens, library: &mut Library, accounts: &[(String, String)], progress: Option<(u32, u32)>, density: beavermail_core::Density) -> Vec<Action> {
    let mut actions = Vec::new();
    // Out of `library` while drawn, so the filters can change as the files are shown; put back at the end.
    let files = library.files.take().unwrap_or_default();
    let mut counts: HashMap<Kind, u32> = HashMap::new();
    for file in files.iter().filter(|f| library.account.as_ref().is_none_or(|a| *a == f.account)) {
        *counts.entry(Kind::All).or_default() += 1;
        *counts.entry(Kind::of(&file.file)).or_default() += 1;
    }
    let frame = egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::same(12));
    egui::Panel::left("library_kinds").frame(frame).default_size(260.0).size_range(210.0..=340.0).show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            theme::dense(ui, density);
            for kind in Kind::ALL {
                if views::icon_row(ui, t, kind.title(), kind.icon(), counts.get(&kind).copied().unwrap_or(0), library.kind == kind).clicked() {
                    library.kind = kind;
                }
            }
            if accounts.len() >= 2 {
                ui.add_space(16.0);
                ui.label(RichText::new("Accounts").size(13.0).color(t.text_dim));
                ui.add_space(4.0);
                if views::icon_row(ui, t, "Every account", Icon::Inbox, 0, library.account.is_none()).clicked() {
                    library.account = None;
                }
                for (key, address) in accounts {
                    if views::icon_row(ui, t, address, Icon::Mail, 0, library.account.as_ref() == Some(key)).clicked() {
                        library.account = Some(key.clone());
                    }
                }
            }
        });
    });

    let shown = shown(&files, library);
    // The file picked stays only while it is shown.
    if library.picked.is_some_and(|p| !shown.iter().any(|f| key(f) == p)) {
        library.picked = None;
    }
    let picked = library.picked.and_then(|p| shown.iter().find(|f| key(f) == p).copied());
    let frame = egui::Frame::new().fill(t.surface).inner_margin(egui::Margin::same(28));
    egui::Panel::right("library_file").frame(frame).default_size(400.0).size_range(320.0..=720.0).show(ui, |ui| match picked {
        Some(file) => {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let multiple = accounts.len() >= 2;
                let address = accounts.iter().find(|(k, _)| *k == file.account).map(|(_, a)| a.as_str());
                if let Some(action) = detail(ui, t, file, library.thumbs.get(&key(file)), address.filter(|_| multiple)) {
                    actions.push(action);
                }
            });
        }
        None => {
            views::empty_state(ui, t, "No file picked", "Pick a file to see it, open it or save it.", None);
        }
    });

    let mut picked_now = library.picked;
    let mut wanted: Vec<Key> = Vec::new();
    let frame = egui::Frame::new().fill(t.surface);
    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            let tools = 2.0 * theme::CONTROL_H + 150.0 + 3.0 * ui.spacing().item_spacing.x + 12.0;
            views::search_box(ui, t, &mut library.query, "Search by name, sender or subject", (ui.available_width() - tools).max(160.0), false);
            egui::ComboBox::from_id_salt("library_sort").selected_text(library.sort.title()).width(150.0).show_ui(ui, |ui| {
                for sort in [Sort::Newest, Sort::Largest, Sort::Name] {
                    ui.selectable_value(&mut library.sort, sort, sort.title());
                }
            });
            if icons::button(ui, t, Icon::List, "Show as a list", true, !library.grid).clicked() {
                library.grid = false;
            }
            if icons::button(ui, t, Icon::LayoutGrid, "Show as a grid of pictures", true, library.grid).clicked() {
                library.grid = true;
            }
        });
        ui.add_space(6.0);
        let counted = progress.filter(|(_, total)| *total > 0).map_or(String::new(), |(done, total)| format!(": {} of {} messages", views::thousands(done), views::thousands(total)));
        if files.is_empty()
            && progress.is_some()
        {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.35).max(24.0));
                views::slow_spinner(ui, 24.0);
                ui.add_space(8.0);
                ui.heading("Looking for attachments");
                ui.label(RichText::new(format!("Your mail is being downloaded and read{counted}. Files show here as they are found.")).color(t.text_dim));
            });
            return;
        }
        if progress.is_some() {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                views::slow_spinner(ui, 14.0);
                ui.label(RichText::new(format!("More may come: mail is still being downloaded and read{counted}")).size(13.0).color(t.text_dim));
            });
            ui.add_space(4.0);
        }
        if shown.is_empty() {
            let (title, about) = if !files.is_empty() {
                ("No file matches", "Try another name or sender, or another kind of file.")
            } else {
                ("No attachments yet", "The files of mail downloaded to this computer show here.")
            };
            views::empty_state(ui, t, title, about, None);
            return;
        }
        let columns = if library.grid { ((ui.available_width() - 16.0) / TILE_W).floor().max(1.0) as usize } else { 1 };
        // Arrows move through the files, Enter opens the one picked; not while typing in the search.
        if !ui.ctx().egui_wants_keyboard_input() {
            let at = picked_now.and_then(|p| shown.iter().position(|f| key(f) == p));
            let step = ui.input(|i| {
                let mut step = 0isize;
                if i.key_pressed(KeyCode::ArrowDown) {
                    step += columns as isize;
                }
                if i.key_pressed(KeyCode::ArrowUp) {
                    step -= columns as isize;
                }
                if library.grid && i.key_pressed(KeyCode::ArrowRight) {
                    step += 1;
                }
                if library.grid && i.key_pressed(KeyCode::ArrowLeft) {
                    step -= 1;
                }
                step
            });
            if step != 0 {
                let next = at.map_or(0, |at| (at as isize + step).clamp(0, shown.len() as isize - 1) as usize);
                picked_now = Some(key(shown[next]));
            }
            if let Some(p) = picked_now.filter(|_| ui.input(|i| i.key_pressed(KeyCode::Enter))) {
                actions.push(Action::Open(p));
            }
        }
        let thumbs = &library.thumbs;
        let mut picture = |file: &LibraryFile| -> Option<egui::TextureHandle> {
            match thumbs.get(&key(file)) {
                Some(Thumb::Ready(texture)) => Some(texture.clone()),
                Some(_) => None,
                None => {
                    if previewable(&file.file) {
                        wanted.push(key(file));
                    }
                    None
                }
            }
        };
        if library.grid {
            let rows = shown.len().div_ceil(columns);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, TILE_H, rows, |ui, range| {
                for row in range {
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for file in shown.iter().skip(row * columns).take(columns) {
                            let here = picked_now == Some(key(file));
                            let response = tile(ui, t, file, picture(file).as_ref(), here);
                            if response.clicked() {
                                picked_now = Some(key(file));
                            }
                            if response.double_clicked() {
                                actions.push(Action::Open(key(file)));
                            }
                        }
                    });
                }
            });
        } else {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, shown.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for file in &shown[range] {
                    let here = picked_now == Some(key(file));
                    let response = file_row(ui, t, file, picture(file).as_ref(), here);
                    if response.clicked() {
                        picked_now = Some(key(file));
                    }
                    if response.double_clicked() {
                        actions.push(Action::Open(key(file)));
                    }
                }
            });
        }
    });
    // The picture of the file picked, for the preview, even when its row is not on screen.
    if let Some(file) = picked.filter(|f| previewable(&f.file) && !library.thumbs.contains_key(&key(f))) {
        wanted.push(key(file));
    }
    library.picked = picked_now;
    library.files = Some(files);
    actions.extend(wanted.into_iter().map(Action::Load));
    actions
}

/// "2026-11-25": the day the message came.
fn day(timestamp: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(timestamp, 0).single().map_or_else(String::new, |d| d.format("%Y-%m-%d").to_string())
}

/// The square at the start of a row or tile: the picture, or the icon of the file's kind.
fn lead(ui: &Ui, t: &Tokens, rect: Rect, file: &LibraryFile, picture: Option<&egui::TextureHandle>, icon_size: f32) {
    ui.painter().rect_filled(rect, CornerRadius::same(CONTROL_RADIUS), t.input);
    match picture {
        Some(texture) => {
            let size = texture.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let fitted = Rect::from_center_size(rect.center(), size * scale);
            egui::Image::from_texture(texture).fit_to_exact_size(fitted.size()).corner_radius(CONTROL_RADIUS).paint_at(ui, fitted);
        }
        None => Kind::of(&file.file).icon().image(icon_size, t.text_dim).paint_at(ui, Rect::from_center_size(rect.center(), vec2(icon_size, icon_size))),
    }
}

fn file_row(ui: &mut Ui, t: &Tokens, file: &LibraryFile, picture: Option<&egui::TextureHandle>, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let card = rect.shrink2(vec2(8.0, 2.0));
    views::row_background(ui, card, &response, selected, t);
    lead(ui, t, Rect::from_center_size(pos2(card.left() + 12.0 + LEAD / 2.0, card.center().y), vec2(LEAD, LEAD)), file, picture, icons::SIZE);
    let size = ui.painter().layout_no_wrap(crate::attachments::human_size(file.file.size), FontId::proportional(13.0), t.text_dim);
    let date = ui.painter().layout_no_wrap(day(file.timestamp), FontId::proportional(13.0), t.text_dim);
    let right = card.right() - 12.0;
    let side = size.size().x.max(date.size().x);
    let left = card.left() + 12.0 + LEAD + 12.0;
    let width = (right - side - 16.0 - left).max(40.0);
    let name = crate::attachments::safe_name(&file.file.name);
    let title = views::truncated(ui, &name, FontId::proportional(15.0), t.text, width);
    let line = if file.subject.is_empty() { file.from.clone() } else { format!("{}  ·  {}", file.from, file.subject) };
    let under = views::truncated(ui, &line, FontId::proportional(13.0), t.text_dim, width);
    let top = card.center().y - (title.size().y + 2.0 + under.size().y) / 2.0;
    let title_h = title.size().y;
    ui.painter().galley(pos2(left, top), title, t.text);
    ui.painter().galley(pos2(left, top + title_h + 2.0), under, t.text_dim);
    let date_h = date.size().y;
    ui.painter().galley(pos2(right - date.size().x, top), date, t.text_dim);
    ui.painter().galley(pos2(right - size.size().x, top + date_h + 2.0), size, t.text_dim);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &name));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn tile(ui: &mut Ui, t: &Tokens, file: &LibraryFile, picture: Option<&egui::TextureHandle>, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(TILE_W, TILE_H), Sense::click());
    let card = rect.shrink(4.0);
    views::row_background(ui, card, &response, selected, t);
    let preview = Rect::from_min_size(card.min + vec2(8.0, 8.0), vec2(card.width() - 16.0, 112.0));
    lead(ui, t, preview, file, picture, 32.0);
    let name = crate::attachments::safe_name(&file.file.name);
    let title = views::truncated(ui, &name, FontId::proportional(14.0), t.text, preview.width());
    let title_h = title.size().y;
    ui.painter().galley(pos2(preview.left(), preview.bottom() + 8.0), title, t.text);
    let size = views::truncated(ui, &crate::attachments::human_size(file.file.size), FontId::proportional(13.0), t.text_dim, preview.width());
    ui.painter().galley(pos2(preview.left(), preview.bottom() + 8.0 + title_h + 2.0), size, t.text_dim);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &name));
    response.on_hover_text(&file.file.name).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The file picked: its picture or icon, what it is, where it came from, and what can be done.
/// `account`: its account's address, named when there are several.
fn detail(ui: &mut Ui, t: &Tokens, file: &LibraryFile, thumb: Option<&Thumb>, account: Option<&str>) -> Option<Action> {
    let mut action = None;
    let width = ui.available_width();
    match thumb {
        Some(Thumb::Ready(texture)) => {
            let size = texture.size_vec2();
            let scale = (width / size.x).min(320.0 / size.y).min(1.0);
            ui.add(egui::Image::from_texture(texture).fit_to_exact_size(size * scale).corner_radius(CONTROL_RADIUS));
        }
        Some(Thumb::Loading) => {
            ui.allocate_ui_with_layout(vec2(width, 160.0), Layout::centered_and_justified(egui::Direction::TopDown), |ui| {
                views::slow_spinner(ui, 24.0);
            });
        }
        _ => {
            let (rect, _) = ui.allocate_exact_size(vec2(width, 160.0), Sense::hover());
            lead(ui, t, rect, file, None, 48.0);
        }
    }
    ui.add_space(16.0);
    let name = crate::attachments::safe_name(&file.file.name);
    ui.add(egui::Label::new(RichText::new(&name).size(theme::SUBTITLE).strong().color(t.text)).wrap());
    ui.label(RichText::new(format!("{}  ·  {}", crate::attachments::human_size(file.file.size), file.file.mime)).color(t.text_dim));
    ui.add_space(16.0);
    let fact = |ui: &mut Ui, label: &str, value: &str| {
        ui.label(RichText::new(label).size(13.0).color(t.text_dim));
        ui.add(egui::Label::new(RichText::new(value).color(t.text)).wrap());
        ui.add_space(8.0);
    };
    fact(ui, "From", &file.from);
    if !file.subject.is_empty() {
        fact(ui, "Subject", &file.subject);
    }
    fact(ui, "Received", &day(file.timestamp));
    let folder = views::folder_title(&file.folder, beavermail_core::Role::Other, None);
    fact(ui, "Folder", &account.map_or(folder.clone(), |a| format!("{folder}  ·  {a}")));
    ui.add_space(12.0);
    ui.horizontal_wrapped(|ui| {
        if views::primary_button(ui, t, "Open", true).on_hover_text("Open with the app your computer uses for it (Enter)").clicked() {
            action = Some(Action::Open(key(file)));
        }
        if ui.button("Save as…").clicked() {
            action = Some(Action::Save(key(file)));
        }
        if ui.button("Go to message").clicked() {
            action = Some(Action::GoTo(key(file)));
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, mime: &str) -> Attachment {
        Attachment { index: 0, name: name.into(), mime: mime.into(), size: 10 }
    }

    #[test]
    fn files_are_sorted_into_kinds_by_type_or_name() {
        assert_eq!(Kind::of(&file("a.png", "image/png")), Kind::Images);
        assert_eq!(Kind::of(&file("scan.PDF", "application/octet-stream")), Kind::Pdf);
        assert_eq!(Kind::of(&file("plan.docx", "application/octet-stream")), Kind::Documents);
        assert_eq!(Kind::of(&file("notes", "text/plain")), Kind::Documents);
        assert_eq!(Kind::of(&file("archive.zip", "application/zip")), Kind::Other);
        assert!(previewable(&file("a.JPG", "application/octet-stream")));
        assert!(!previewable(&file("a.gif", "image/gif")));
    }

    #[test]
    fn the_files_shown_follow_kind_account_words_and_order() {
        let entry = |id: u64, name: &str, size: u64, account: &str| LibraryFile {
            message: MessageId(id),
            file: Attachment { index: 0, name: name.into(), mime: "application/octet-stream".into(), size },
            account: account.into(),
            folder: "INBOX".into(),
            from: "Ada Lovelace".into(),
            subject: "Engine notes".into(),
            timestamp: id as i64,
        };
        let mut library = Library { files: Some(vec![entry(3, "b.pdf", 5, "one"), entry(2, "a.zip", 50, "one"), entry(1, "c.pdf", 1, "two")]), ..Library::default() };
        let names = |l: &Library| l.shown().iter().map(|f| f.file.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&library), ["b.pdf", "a.zip", "c.pdf"]);
        library.kind = Kind::Pdf;
        assert_eq!(names(&library), ["b.pdf", "c.pdf"]);
        library.account = Some("two".into());
        assert_eq!(names(&library), ["c.pdf"]);
        (library.kind, library.account, library.query) = (Kind::All, None, "lovelace ENGINE zip".into());
        assert_eq!(names(&library), ["a.zip"]);
        (library.query, library.sort) = (String::new(), Sort::Largest);
        assert_eq!(names(&library), ["a.zip", "b.pdf", "c.pdf"]);
        library.sort = Sort::Name;
        assert_eq!(names(&library), ["a.zip", "b.pdf", "c.pdf"]);
    }
}
