//! The compose window (new message, reply, forward): addresses, subject and a plain-text body.

use beavermail_core::{MessageBody, MessageSummary, Outgoing};
use eframe::egui::{self, Align, Align2, Frame, Key, Layout, Modifiers, RichText, TextEdit, Ui, vec2};

use crate::drafts::{self, DraftData, DraftFile};
use crate::icons::Icon;
use crate::theme::{self, Tokens};
use crate::views;

const ROW_H: f32 = 38.0;
const LABEL_W: f32 = 76.0;

pub struct Compose {
    /// Serial of the sending account.
    pub from: u64,
    to: String,
    cc: String,
    subject: String,
    body: String,
    /// Window title: "New message", "Reply" or "Forward".
    title: &'static str,
    in_reply_to: Option<String>,
    references: Vec<String>,
    pub sending: bool,
    /// When the message is handed to the server; until then Undo takes it back.
    pub send_at: Option<std::time::Instant>,
    pub error: Option<String>,
    focus_to: bool,
    /// Put the caret at the top of the body on the first frame (replies and forwards).
    focus_body: bool,
    pub files: Vec<AttachedFile>,
    /// Names the draft and its attachments in the vault.
    pub id: String,
    /// The account (its key) whose drafts folder holds this draft.
    pub key: String,
    next_file: u32,
    /// Original attachments of a forwarded message still to be copied: (position in the message, where).
    pub carry: Vec<(u32, std::path::PathBuf)>,
    /// What the text looked like when last seen, when it last changed, and when it was last saved.
    seen: u64,
    changed_at: std::time::Instant,
    saved: u64,
    /// What the text looked like when the copy on the server was last stored (0: never), and whether
    /// a copy is being stored right now.
    pub server_saved: u64,
    pub pushing: bool,
    /// A draft of this message exists (or may exist) on the server: it was saved, pushed, or reopened.
    touched: bool,
    /// Show the message as the recipient will read it instead of the text being edited.
    pub preview: bool,
}

pub enum Outcome {
    Keep,
    Send,
    Discard,
    /// Close the window and keep what was written as a draft.
    Close,
    /// Open the dialog to pick files to attach.
    Attach,
    /// Take back a message that is waiting to be sent.
    Undo,
}

/// A file that goes with the message. Its content is a copy in the vault (under the draft), so it
/// survives the original moving; `ready` is false while the copy is being made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachedFile {
    pub name: String,
    pub size: u64,
    pub vault: std::path::PathBuf,
    pub ready: bool,
}

/// Above this many bytes many providers refuse a message (a common limit is 25 MB).
pub const SIZE_WARNING: u64 = 25 * 1024 * 1024;

impl Compose {
    pub fn new(from: u64) -> Self {
        Self {
            from,
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            title: "New message",
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
            send_at: None,
            error: None,
            focus_to: true,
            focus_body: false,
            files: Vec::new(),
            id: drafts::new_id(),
            key: String::new(),
            next_file: 0,
            carry: Vec::new(),
            seen: 0,
            changed_at: std::time::Instant::now(),
            saved: 0,
            server_saved: 0,
            pushing: false,
            touched: false,
            preview: false,
        }
        .settled()
    }

    /// Treats the text as it is now as already saved: an untouched reply is not worth a draft.
    fn settled(mut self) -> Self {
        self.saved = self.fingerprint();
        self.seen = self.saved;
        self
    }

    /// A number for what is written, to tell when it changes.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (self.from, &self.to, &self.cc, &self.subject, &self.body, self.title, &self.in_reply_to, &self.references).hash(&mut hasher);
        for f in &self.files {
            (&f.name, f.size, &f.vault).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Call once per frame. True when the text changed and has stayed still for `quiet`: time to save a draft.
    pub fn autosave_due(&mut self, quiet: std::time::Duration) -> bool {
        let now = self.fingerprint();
        if now != self.seen {
            self.seen = now;
            self.changed_at = std::time::Instant::now();
        }
        now != self.saved && self.changed_at.elapsed() >= quiet
    }

    pub fn mark_saved(&mut self) {
        self.saved = self.fingerprint();
        self.touched = true;
    }

    /// Whether discarding this message must also look for a copy on the server.
    pub fn may_be_on_server(&self) -> bool {
        self.touched || self.server_saved != 0 || self.pushing
    }

    /// Whether the text, as it is now, is not yet on the server and has been still for `quiet`.
    pub fn server_due(&self, quiet: std::time::Duration) -> bool {
        !self.pushing && !self.is_empty() && self.fingerprint() != self.server_saved && self.changed_at.elapsed() >= quiet
    }

    /// The same number [`Compose::autosave_due`] uses, to remember what was stored.
    pub fn snapshot(&self) -> u64 {
        self.fingerprint()
    }

    /// The text has changed since the last save.
    pub fn unsaved(&self) -> bool {
        self.fingerprint() != self.saved
    }

    pub fn to_draft(&self, updated: i64) -> DraftData {
        DraftData {
            id: self.id.clone(),
            title: self.title.to_owned(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            subject: self.subject.clone(),
            body: self.body.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            files: self.files.iter().filter(|f| f.ready).map(|f| DraftFile { name: f.name.clone(), size: f.size, path: f.vault.clone() }).collect(),
            updated,
        }
    }

    /// A window reopened on a saved draft, sending from account `from`.
    pub fn from_draft(from: u64, draft: DraftData) -> Self {
        let title = match draft.title.as_str() {
            "Reply" => "Reply",
            "Reply all" => "Reply all",
            "Forward" => "Forward",
            _ => "New message",
        };
        Self {
            to: draft.to,
            cc: draft.cc,
            subject: draft.subject,
            body: draft.body,
            title,
            in_reply_to: draft.in_reply_to,
            references: draft.references,
            files: draft.files.into_iter().map(|f| AttachedFile { name: f.name, size: f.size, vault: f.path, ready: true }).collect(),
            id: draft.id,
            next_file: 1000, // above any slot number the draft used before
            focus_to: false,
            focus_body: true,
            touched: true,
            ..Self::new(from)
        }
        .settled()
    }

    /// Makes room for a file and says where its copy goes in the vault.
    pub fn reserve(&mut self, account_key: &str, name: &str, size: u64, ready: bool) -> std::path::PathBuf {
        let vault = drafts::files_dir(account_key, &self.id).join(format!("{}-{}", self.next_file, name));
        self.next_file += 1;
        self.files.push(AttachedFile { name: name.to_owned(), size, vault: vault.clone(), ready });
        vault
    }

    /// Adds placeholders for `files` (see [`usable`]) and returns, for each, where to copy it from and to.
    /// A file with the same name and size as one already attached is skipped.
    pub fn add_pending(&mut self, account_key: &str, files: Vec<(std::path::PathBuf, String, u64)>) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
        let mut jobs = Vec::new();
        for (source, name, size) in files {
            if self.files.iter().any(|f| f.name == name && f.size == size) {
                continue;
            }
            jobs.push((source, self.reserve(account_key, &name, size, false)));
        }
        jobs
    }

    /// A copy was made (`ok`) or failed (the placeholder goes away).
    pub fn file_done(&mut self, vault: &std::path::Path, ok: bool) {
        if ok {
            if let Some(f) = self.files.iter_mut().find(|f| f.vault == vault) {
                f.ready = true;
            }
        } else {
            self.files.retain(|f| f.vault != vault);
        }
    }

    /// Files still being copied.
    pub fn pending(&self) -> usize {
        self.files.iter().filter(|f| !f.ready).count()
    }

    /// A reply to `message`, to the sender only or to everyone but `own` (the sending address).
    pub fn reply(from: u64, own: &str, message: &MessageSummary, body: &MessageBody, all: bool) -> Self {
        let mut to: Vec<String> = if body.reply_to.is_empty() { vec![sender(message)] } else { body.reply_to.clone() };
        let mut cc: Vec<String> = Vec::new();
        if all {
            let seen: Vec<String> = to.iter().map(|a| bare_address(a)).collect();
            for other in body.to.iter().chain(&body.cc) {
                let bare = bare_address(other);
                if bare != bare_address(own) && !seen.contains(&bare) && !cc.iter().any(|c| bare_address(c) == bare) {
                    cc.push(other.clone());
                }
            }
        }
        to.retain(|a| !a.trim().is_empty());
        let mut references = body.references.clone();
        if !body.message_id.is_empty() {
            references.push(body.message_id.clone());
        }
        Self {
            to: to.join(", "),
            cc: cc.join(", "),
            subject: prefixed("Re", &message.subject),
            body: format!("\n\nOn {}, {} wrote:\n{}", message.received, sender(message), quote(&body.text)),
            title: if all { "Reply all" } else { "Reply" },
            in_reply_to: Some(body.message_id.clone()).filter(|id| !id.is_empty()),
            references,
            focus_to: false,
            focus_body: true,
            ..Self::new(from)
        }
        .settled()
    }

    /// `message` forwarded inline, with its attachments: their copies are made in the background
    /// (see [`Compose::carry`]).
    pub fn forward(from: u64, account_key: &str, message: &MessageSummary, body: &MessageBody, attachments: &[beavermail_core::Attachment]) -> Self {
        let mut header = format!("---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\n", sender(message), message.received, message.subject);
        if !body.to.is_empty() {
            header.push_str(&format!("To: {}\n", body.to.join(", ")));
        }
        let mut forward = Self {
            subject: prefixed("Fwd", &message.subject),
            body: format!("\n\n{header}\n{}", body.text),
            title: "Forward",
            focus_to: true,
            ..Self::new(from)
        };
        for attachment in attachments {
            let to = forward.reserve(account_key, &crate::attachments::safe_name(&attachment.name), attachment.size, false);
            forward.carry.push((attachment.index, to));
        }
        forward.settled()
    }

    /// Nothing typed yet, so closing loses nothing.
    pub fn is_empty(&self) -> bool {
        [&self.to, &self.cc, &self.subject, &self.body].iter().all(|t| t.trim().is_empty()) && self.files.is_empty()
    }

    pub fn attached_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// What the sender needs to read the files: where in the vault, and the name the recipient sees.
    pub fn file_list(&self) -> Vec<(std::path::PathBuf, String)> {
        self.files.iter().map(|f| (f.vault.clone(), f.name.clone())).collect()
    }

    pub fn outgoing(&self, from_address: &str, rich: bool) -> Outgoing {
        Outgoing {
            from: from_address.to_owned(),
            to: addresses(&self.to),
            cc: addresses(&self.cc),
            subject: self.subject.trim().to_owned(),
            body: self.body.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: Vec::new(), // read from `files` when the message is sent
            rich,
        }
    }
}

/// The files among `paths` that can be attached (folders and missing files are left out), with the
/// name the recipient will see and the size.
pub fn usable(paths: impl IntoIterator<Item = std::path::PathBuf>) -> Vec<(std::path::PathBuf, String, u64)> {
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
            let name = crate::attachments::safe_name(&path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()));
            Some((path, name, meta.len()))
        })
        .collect()
}

/// "Name <address>" for a message's sender.
fn sender(message: &MessageSummary) -> String {
    match (message.from.trim(), message.address.trim()) {
        ("", address) => address.to_owned(),
        (name, "") => name.to_owned(),
        (name, address) if name == address => address.to_owned(),
        (name, address) => format!("{name} <{address}>"),
    }
}

/// The lower-case address inside "Name <address>", or the text itself.
fn bare_address(text: &str) -> String {
    let inner = match (text.rfind('<'), text.rfind('>')) {
        (Some(open), Some(close)) if open < close => &text[open + 1..close],
        _ => text,
    };
    inner.trim().to_lowercase()
}

/// "Re: subject" without stacking prefixes.
fn prefixed(prefix: &str, subject: &str) -> String {
    let subject = subject.trim();
    let has = subject.len() > prefix.len() && subject[..prefix.len()].eq_ignore_ascii_case(prefix) && subject[prefix.len()..].starts_with(':');
    if has { subject.to_owned() } else { format!("{prefix}: {subject}") }
}

/// Every line of `text` marked as a quotation.
fn quote(text: &str) -> String {
    text.trim_end().lines().map(|l| if l.is_empty() { ">".to_owned() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

/// The window system does not deliver dropped files here: the toolkit has no drag and drop on Wayland.
fn on_wayland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// Files dropped onto the window this frame.
pub fn dropped(ctx: &egui::Context) -> Vec<std::path::PathBuf> {
    ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect())
}

/// Files are being dragged over the window right now.
pub fn hovering_files(ctx: &egui::Context) -> bool {
    ctx.input(|i| !i.raw.hovered_files.is_empty())
}

/// A tinted plate with a sentence over `rect`, drawn above everything else.
pub fn drop_hint(ctx: &egui::Context, t: &Tokens, rect: egui::Rect, text: &str) {
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_hint")));
    painter.rect_filled(rect, egui::CornerRadius::same(theme::CONTROL_RADIUS), t.accent.gamma_multiply(0.22));
    painter.rect_stroke(rect.shrink(2.0), egui::CornerRadius::same(theme::CONTROL_RADIUS), egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, egui::FontId::proportional(18.0), t.accent);
}

/// Splits a recipient field on commas, semicolons and newlines.
// shortcut: a comma inside a quoted display name ("Doe, John" <j@x.org>) splits it wrongly; write a real parser with contacts.
pub fn addresses(text: &str) -> Vec<String> {
    text.split([',', ';', '\n']).map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned).collect()
}

/// A dim label in a fixed-width column, left-aligned so the fields start at the same x.
fn label(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.allocate_ui_with_layout(vec2(LABEL_W, ROW_H), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(vec2(LABEL_W, ROW_H));
        ui.label(RichText::new(text).color(t.text_dim));
    });
}

/// A labelled, borderless single-line field followed by a divider.
fn row(ui: &mut Ui, t: &Tokens, name: &str, text: &mut String, hint: &str) -> egui::Response {
    let response = ui
        .horizontal(|ui| {
            label(ui, t, name);
            ui.add(
                TextEdit::singleline(text)
                    .frame(Frame::NONE)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY)
                    .min_size(vec2(0.0, ROW_H))
                    .vertical_align(Align::Center),
            )
        })
        .inner;
    ui.separator();
    response
}

/// A Markdown edit made by a toolbar button.
#[derive(Clone, Copy)]
enum Format {
    /// Wrap the selection (or the cursor) in these marks.
    Wrap(&'static str),
    /// Start every selected line with this prefix.
    Lines(&'static str),
    Link,
}

/// Applies `format` to the characters `from..to` of `text`; returns the selection to restore.
fn format_text(text: &mut String, from: usize, to: usize, format: Format) -> (usize, usize) {
    let byte = |text: &str, n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    let (from, to) = (from.min(to), from.max(to));
    let (a, b) = (byte(text, from), byte(text, to));
    match format {
        Format::Wrap(mark) => {
            text.insert_str(b, mark);
            text.insert_str(a, mark);
            let n = mark.chars().count();
            (from + n, to + n)
        }
        Format::Link => {
            let label = if a == b { "text" } else { &text[a..b] }.to_owned();
            let url = "https://";
            text.replace_range(a..b, &format!("[{label}]({url})"));
            // Leave the address selected: that is what is left to write.
            let start = from + label.chars().count() + 3;
            (start, start + url.chars().count())
        }
        Format::Lines(prefix) => {
            let start = text[..a].rfind('\n').map_or(0, |i| i + 1);
            let selected = text[start..b].to_owned();
            let prefixed: String = selected.split('\n').map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n");
            let added = prefixed.chars().count() - selected.chars().count();
            text.replace_range(start..b, &prefixed);
            (from + prefix.chars().count(), to + added)
        }
    }
}

/// Clickable but out of the Tab order: Tab goes from the subject straight to the text.
const NO_TAB: egui::Sense = egui::Sense::CLICK;

/// Formatting buttons above the text, and the switch between writing and the recipient's view.
fn toolbar(ui: &mut Ui, ctx: &egui::Context, t: &Tokens, compose: &mut Compose, id: egui::Id) {
    ui.horizontal(|ui| {
        let tools = [
            ("B", "Bold", Format::Wrap("**")),
            ("I", "Italic", Format::Wrap("*")),
            ("Link", "Link", Format::Link),
            ("• List", "Bulleted list", Format::Lines("- ")),
            ("❝ Quote", "Quote", Format::Lines("> ")),
        ];
        for (label, tip, format) in tools {
            let text = match label {
                "B" => RichText::new(label).strong(),
                "I" => RichText::new(label).italics(),
                _ => RichText::new(label),
            };
            if ui.add_enabled(!compose.preview, egui::Button::new(text).frame(false).sense(NO_TAB)).on_hover_text(tip).clicked()
                && let Some(mut state) = TextEdit::load_state(ctx, id)
            {
                let range = state.cursor.char_range().map_or((0, 0), |r| (r.primary.index.into(), r.secondary.index.into()));
                let (from, to) = format_text(&mut compose.body, range.0, range.1, format);
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(from), egui::text::CCursor::new(to))));
                state.store(ctx, id);
                ctx.memory_mut(|m| m.request_focus(id));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for (preview, label) in [(true, "Preview"), (false, "Write")] {
                if ui.add(egui::Button::selectable(compose.preview == preview, label).sense(NO_TAB)).clicked() {
                    compose.preview = preview;
                }
            }
        });
    });
    ui.label(RichText::new("Markdown: sent as plain text with a simple HTML version. No images or tracking.").size(12.5).color(t.text_dim));
}

/// Shows the window. `senders` lists the accounts that can send, as (serial, address).
pub fn window(ctx: &egui::Context, t: &Tokens, compose: &mut Compose, senders: &[(u64, String)], rich: bool) -> Outcome {
    let mut outcome = Outcome::Keep;
    let waiting_for_files = compose.pending() > 0;
    let ready = !compose.sending && !compose.to.trim().is_empty() && !waiting_for_files;
    let shown = egui::Window::new(compose.title)
        .id(egui::Id::new("compose"))
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
        .default_width(620.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.add_enabled_ui(!compose.sending, |ui| {
                ui.horizontal(|ui| {
                    label(ui, t, "From");
                    let current = senders.iter().find(|(s, _)| *s == compose.from).map(|(_, a)| a.as_str()).unwrap_or("");
                    egui::ComboBox::from_id_salt("compose_from").selected_text(current).show_ui(ui, |ui| {
                        for (serial, address) in senders {
                            ui.selectable_value(&mut compose.from, *serial, address);
                        }
                    });
                });
                ui.separator();
                let to = row(ui, t, "To", &mut compose.to, "name@example.com, …");
                if std::mem::take(&mut compose.focus_to) {
                    to.request_focus();
                }
                row(ui, t, "Cc", &mut compose.cc, "");
                row(ui, t, "Subject", &mut compose.subject, "");
                // Leave room for the header rows and buttons so the window never outgrows a small screen.
                let body_h = (ctx.content_rect().height() - 360.0).clamp(120.0, 420.0);
                let id = egui::Id::new("compose_body");
                if rich {
                    toolbar(ui, ctx, t, compose, id);
                }
                egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, true]).show(ui, |ui| {
                    if rich && compose.preview {
                        let blocks = crate::html::blocks(&beavermail_smtp::markdown_html(&compose.body));
                        crate::views::body(ui, t, &blocks);
                        return;
                    }
                    let mut output = TextEdit::multiline(&mut compose.body)
                        .id(id)
                        .frame(Frame::NONE)
                        .hint_text("Write your message")
                        .desired_width(f32::INFINITY)
                        .desired_rows(8)
                        .show(ui);
                    if std::mem::take(&mut compose.focus_body) {
                        output.response.request_focus();
                        output.state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(0))));
                        output.state.store(ctx, id);
                    }
                });
            });
            if !compose.files.is_empty() {
                ui.add_space(6.0);
                let mut remove = None;
                ui.horizontal_wrapped(|ui| {
                    for (i, file) in compose.files.iter().enumerate() {
                        egui::Frame::new()
                            .fill(t.input)
                            .stroke(egui::Stroke::new(1.0, t.border_strong))
                            .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
                            .inner_margin(egui::Margin::symmetric(10, 5))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add(Icon::Paperclip.image(crate::icons::SIZE, t.text_dim));
                                    let shown: String = if file.name.chars().count() > 28 { format!("{}…", file.name.chars().take(27).collect::<String>()) } else { file.name.clone() };
                                    ui.label(shown).on_hover_text(&file.name);
                                    if file.ready {
                                        ui.label(RichText::new(crate::attachments::human_size(file.size)).size(13.0).color(t.text_dim));
                                    } else {
                                        ui.add(egui::Spinner::new().size(12.0));
                                        ui.label(RichText::new("adding…").size(13.0).color(t.text_dim));
                                    }
                                    if !compose.sending && crate::icons::button_small(ui, t, Icon::X, "Remove").clicked() {
                                        remove = Some(i);
                                    }
                                });
                            });
                    }
                });
                if let Some(i) = remove {
                    compose.files.remove(i);
                }
                if compose.attached_size() > SIZE_WARNING {
                    ui.label(RichText::new(format!("The attachments add up to {}. Many providers refuse messages over 25 MB.", crate::attachments::human_size(compose.attached_size()))).size(13.0).color(t.danger));
                }
            }
            if let Some(error) = &compose.error {
                ui.add_space(4.0);
                ui.colored_label(t.danger, error);
            }
            ui.add_space(8.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                let send = views::primary_wide_fixed(ui, t, Icon::Send, if compose.sending { "Sending…" } else { "Send" }, ready, 120.0);
                let send = if waiting_for_files { send.on_disabled_hover_text("Waiting for the attachments to be added") } else { send };
                if send.clicked() {
                    outcome = Outcome::Send;
                }
                if ui.add_enabled(!compose.sending, egui::Button::image_and_text(Icon::Paperclip.image(crate::icons::SIZE, t.text_dim), "Attach")).on_hover_text(if on_wayland() { "Attach files. Dropping files in does not work on Wayland yet; start the app with WAYLAND_DISPLAY unset to use X11." } else { "Attach files, or drop them in the window" }).clicked() {
                    outcome = Outcome::Attach;
                }
                if let Some(at) = compose.send_at {
                    let left = at.saturating_duration_since(std::time::Instant::now());
                    if ui.button(format!("Undo ({}s)", left.as_secs() + 1)).on_hover_text("Take the message back before it is sent").clicked() {
                        outcome = Outcome::Undo;
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                } else if compose.sending {
                    ui.spinner();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!compose.sending, egui::Button::image_and_text(Icon::Trash.image(crate::icons::SIZE, t.text_dim), "Discard")).on_hover_text("Delete this message and its draft (Esc or Ctrl+Shift+D)").clicked() {
                        outcome = Outcome::Discard;
                    }
                    if ui.add_enabled(!compose.sending, egui::Button::image_and_text(Icon::X.image(crate::icons::SIZE, t.text_dim), "Close")).on_hover_text("Close and keep it as a draft").clicked() {
                        outcome = Outcome::Close;
                    }
                    // Only when there is room: squeezed in, it would be drawn over the buttons.
                    let hint = RichText::new("Saved as a draft while you write").size(12.5).color(t.text_dim);
                    let width = ui.painter().layout_no_wrap("Saved as a draft while you write".to_owned(), egui::FontId::proportional(12.5), t.text_dim).size().x;
                    if ui.available_width() > width + 24.0 {
                        ui.label(hint);
                    }
                });
            });
        });
    // While files are being dragged over the app, say where they would go.
    if let Some(window) = shown
        && hovering_files(ctx)
    {
        drop_hint(ctx, t, window.response.rect, "Drop files to attach them");
    }
    if ready && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
        outcome = Outcome::Send;
    }
    if !compose.sending && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::D)) {
        outcome = Outcome::Discard;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_edits_wrap_prefix_and_link() {
        let mut s = "héllo wörld".to_owned();
        assert_eq!(format_text(&mut s, 6, 11, Format::Wrap("**")), (8, 13));
        assert_eq!(s, "héllo **wörld**");
        let mut s = "one\ntwo\nthree".to_owned();
        format_text(&mut s, 5, 9, Format::Lines("- "));
        assert_eq!(s, "one\n- two\n- three");
        let mut s = "see docs".to_owned();
        let (a, b) = format_text(&mut s, 4, 8, Format::Link);
        assert_eq!(s, "see [docs](https://)");
        assert_eq!(s.chars().skip(a).take(b - a).collect::<String>(), "https://");
        let mut s = String::new();
        format_text(&mut s, 0, 0, Format::Wrap("*"));
        assert_eq!(s, "**");
    }

    #[test]
    fn recipient_fields_split_on_separators() {
        assert_eq!(addresses("a@x.org, b@x.org;\n c@x.org ,,"), ["a@x.org", "b@x.org", "c@x.org"]);
        assert!(addresses("  ").is_empty());
    }

    fn message() -> (MessageSummary, MessageBody) {
        let summary = MessageSummary {
            id: beavermail_core::MessageId(1),
            from: "Ada Lovelace".into(),
            address: "ada@example.org".into(),
            subject: "Lunch".into(),
            preview: String::new(),
            received: "2026-10-13 12:00".into(),
            timestamp: 0,
            has_attachments: false,
            message_id: String::new(),
            unread: false,
        };
        let body = MessageBody {
            text: "See you at noon.\n\nBring the notes.".into(),
            to: vec!["me@example.org".into(), "Grace <grace@example.org>".into()],
            cc: vec!["Linus <linus@example.org>".into(), "GRACE@example.org".into()],
            reply_to: vec![],
            message_id: "m2@host".into(),
            references: vec!["m1@host".into()],
            html: None,
            attachments: Vec::new(),
            ..MessageBody::default()
        };
        (summary, body)
    }

    #[test]
    fn a_reply_goes_to_the_sender_and_keeps_the_thread() {
        let (summary, body) = message();
        let c = Compose::reply(0, "me@example.org", &summary, &body, false);
        assert_eq!(c.to, "Ada Lovelace <ada@example.org>");
        assert!(c.cc.is_empty());
        assert_eq!(c.subject, "Re: Lunch");
        assert!(c.body.starts_with("\n\nOn 2026-10-13 12:00, Ada Lovelace <ada@example.org> wrote:\n> See you at noon.\n>\n> Bring the notes."), "{}", c.body);
        let out = c.outgoing("me@example.org", false);
        assert_eq!(out.in_reply_to.as_deref(), Some("m2@host"));
        assert_eq!(out.references, ["m1@host", "m2@host"]);
    }

    #[test]
    fn reply_all_drops_me_and_duplicates() {
        let (summary, body) = message();
        let c = Compose::reply(0, "Me@Example.org", &summary, &body, true);
        assert_eq!(c.to, "Ada Lovelace <ada@example.org>");
        assert_eq!(c.cc, "Grace <grace@example.org>, Linus <linus@example.org>");
    }

    #[test]
    fn reply_to_header_wins_and_prefixes_do_not_stack() {
        let (mut summary, mut body) = message();
        body.reply_to = vec!["list@example.org".into()];
        summary.subject = "RE: Lunch".into();
        let c = Compose::reply(0, "me@example.org", &summary, &body, false);
        assert_eq!(c.to, "list@example.org");
        assert_eq!(c.subject, "RE: Lunch");
    }

    #[test]
    fn a_forward_quotes_the_original_without_threading() {
        let (summary, body) = message();
        let c = Compose::forward(0, "me", &summary, &body, &[]);
        assert_eq!(c.subject, "Fwd: Lunch");
        assert!(c.to.is_empty());
        assert!(c.body.contains("---------- Forwarded message ----------\nFrom: Ada Lovelace <ada@example.org>"));
        assert!(c.body.ends_with("Bring the notes."));
        assert!(c.outgoing("me@example.org", false).in_reply_to.is_none());
    }

    #[test]
    fn only_files_can_be_attached_and_the_same_file_twice_is_skipped() {
        let dir = std::env::temp_dir().join(format!("beavermail-attach-compose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"12345").unwrap();
        std::fs::write(dir.join("b.txt"), b"123").unwrap();
        let found = usable([dir.join("a.txt"), dir.join("sub"), dir.join("missing"), dir.join("b.txt")]);
        assert_eq!(found.iter().map(|(_, n, s)| (n.as_str(), *s)).collect::<Vec<_>>(), [("a.txt", 5), ("b.txt", 3)]);

        let mut c = Compose::new(0);
        assert!(c.is_empty());
        let jobs = c.add_pending("me@example.org", found.clone());
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].1.starts_with(drafts::files_dir("me@example.org", &c.id)), "{:?}", jobs[0].1);
        assert!(c.add_pending("me@example.org", found).is_empty(), "already attached");
        assert_eq!((c.pending(), c.attached_size()), (2, 8));
        assert!(!c.is_empty(), "files alone are something to lose");
        c.file_done(&jobs[0].1, true);
        c.file_done(&jobs[1].1, false);
        assert_eq!((c.pending(), c.files.len()), (0, 1), "a failed copy leaves no placeholder");
        assert_eq!(c.file_list()[0].1, "a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_draft_is_saved_when_the_text_has_been_still_not_while_it_changes_and_never_untouched() {
        let mut c = Compose::new(0);
        let quiet = std::time::Duration::from_millis(40);
        assert!(!c.autosave_due(quiet), "nothing written");
        c.subject = "Lunch".into();
        assert!(!c.autosave_due(quiet), "just changed");
        std::thread::sleep(quiet * 2);
        assert!(c.autosave_due(quiet), "still for long enough");
        c.mark_saved();
        assert!(!c.autosave_due(quiet), "nothing new since the save");
        c.subject = "Lunch at noon".into();
        assert!(!c.autosave_due(quiet), "changed again");
        assert!(c.unsaved());
    }

    #[test]
    fn only_a_message_that_was_saved_or_reopened_can_have_a_copy_on_the_server() {
        let mut fresh = Compose::new(0);
        assert!(!fresh.may_be_on_server(), "nothing was ever saved");
        fresh.subject = "x".into();
        fresh.mark_saved();
        assert!(fresh.may_be_on_server());
        assert!(Compose::from_draft(0, Compose::new(0).to_draft(1)).may_be_on_server(), "a reopened draft may have a copy");
    }

    #[test]
    fn a_draft_comes_back_as_it_was() {
        let mut c = Compose::new(3);
        c.to = "a@x.org".into();
        c.subject = "Plan".into();
        c.body = "Hello".into();
        let jobs = c.add_pending("me", vec![("/tmp/plan.pdf".into(), "plan.pdf".into(), 9)]);
        c.file_done(&jobs[0].1, true);
        let draft = c.to_draft(1234);
        assert_eq!((draft.updated, draft.files.len()), (1234, 1));
        let back = Compose::from_draft(3, draft);
        assert_eq!((back.id.as_str(), back.to.as_str(), back.subject.as_str(), back.body.as_str()), (c.id.as_str(), "a@x.org", "Plan", "Hello"));
        assert_eq!(back.files[0].name, "plan.pdf");
        assert!(back.files[0].ready && back.pending() == 0);
        assert!(!back.unsaved(), "just opened, nothing to save");
    }

    #[test]
    fn a_forward_reserves_its_attachments_to_be_copied_and_a_reply_does_not() {
        let (summary, body) = message();
        let originals = vec![
            beavermail_core::Attachment { index: 0, name: "plan.pdf".into(), mime: "application/pdf".into(), size: 11 },
            beavermail_core::Attachment { index: 2, name: "../notes.txt".into(), mime: "text/plain".into(), size: 8 },
        ];
        let f = Compose::forward(0, "me", &summary, &body, &originals);
        assert_eq!(f.files.iter().map(|x| (x.name.as_str(), x.ready)).collect::<Vec<_>>(), [("plan.pdf", false), ("notes.txt", false)]);
        assert_eq!(f.carry.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(f.pending(), 2);
        assert!(!f.unsaved(), "untouched, so no draft yet");
        assert!(Compose::reply(0, "me@example.org", &summary, &body, false).files.is_empty());
    }

    #[test]
    fn empty_means_nothing_typed() {
        let mut c = Compose::new(0);
        assert!(c.is_empty());
        c.subject = "Hi".into();
        assert!(!c.is_empty());
    }
}
