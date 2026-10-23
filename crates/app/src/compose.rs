//! The compose window (new message, reply, forward): addresses, subject and a plain-text body.

use beavermail_core::{MessageBody, MessageSummary, Outgoing};
use beavermail_store::Contact;
use eframe::egui::{self, Align, Align2, Frame, Key, Layout, Modifiers, RichText, TextEdit, Ui, vec2};

use crate::drafts::{self, DraftData, DraftFile};
use crate::icons::Icon;
use crate::theme::{self, Tokens};
use crate::views;

const ROW_H: f32 = 38.0;
/// An address chip: its text and remove button with the frame's margin.
const CHIP_H: f32 = 28.0;
/// Show and go to the Cc and Bcc fields, as in Gmail.
const CC_KEY: egui::KeyboardShortcut = egui::KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::C);
const BCC_KEY: egui::KeyboardShortcut = egui::KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::B);
const LABEL_W: f32 = 76.0;

pub struct Compose {
    /// Serial of the sending account.
    pub from: u64,
    to: String,
    cc: String,
    bcc: String,
    /// Cc and Bcc show once asked for, or when they hold an address.
    show_cc: bool,
    show_bcc: bool,
    subject: String,
    body: String,
    /// Window title: "New message", "Reply" or "Forward".
    title: &'static str,
    in_reply_to: Option<String>,
    references: Vec<String>,
    pub sending: bool,
    /// When the message is handed to the server; until then Undo takes it back.
    pub send_at: Option<std::time::Instant>,
    /// The day picked under "Send later", the month its calendar shows (its first day), and the time typed.
    later_day: chrono::NaiveDate,
    later_month: chrono::NaiveDate,
    later_time: String,
    /// The "Pick date & time" dialog is open.
    pub picking: bool,
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
    /// The signature in the body, with the blank lines before it; empty when there is none.
    sig: String,
    /// The account whose signature `sig` is.
    pub sig_from: u64,
    /// Put the caret at the top of the body when it is first drawn (above a signature).
    caret_top: bool,
    /// People to suggest in the address fields, as (name, address), best first.
    pub contacts: std::sync::Arc<Vec<Contact>>,
    /// An address field shows suggestions: Esc closes them, not the window.
    pub suggesting: bool,
    /// Send it encrypted (OpenPGP).
    pub encrypt: bool,
    /// `encrypt` follows whether it can be (every recipient's key known) until the user sets it.
    pub encrypt_auto: bool,
    /// Whether it can be encrypted, or why not; worked out by the app, which knows the keys.
    pub can_encrypt: Result<(), String>,
    /// An answer to an invitation: its iTIP part, sent beside the text.
    pub calendar: Option<String>,
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
    /// Send later, at this moment (seconds since the Unix epoch).
    Schedule(i64),
}

/// What "send later" offers from `now`, as (label, moment): tomorrow morning and afternoon, and
/// Monday morning when that is not tomorrow.
pub fn later_choices(now: chrono::DateTime<chrono::Local>) -> Vec<(String, i64)> {
    use chrono::{Datelike, Duration, Timelike, Weekday};
    let at = |day: chrono::NaiveDate, hour: u32, minute: u32| day.and_hms_opt(hour, minute, 0).and_then(|t| t.and_local_timezone(chrono::Local).earliest()).map(|t| t.timestamp());
    let (today, tomorrow) = (now.date_naive(), now.date_naive() + Duration::days(1));
    let mut out = Vec::new();
    // Later today: in three hours, on the next half hour, while that is still a working day's hour.
    let minutes = (now.hour() * 60 + now.minute() + 180).div_ceil(30) * 30;
    if minutes < 21 * 60 {
        out.push(("Later today".to_owned(), at(today, minutes / 60, minutes % 60)));
    }
    // Not when "later today" is already about the evening.
    if minutes < 17 * 60 {
        out.push(("This evening".to_owned(), at(today, 18, 0)));
    }
    out.push(("Tomorrow morning".to_owned(), at(tomorrow, 8, 0)));
    out.push(("Tomorrow afternoon".to_owned(), at(tomorrow, 13, 0)));
    if tomorrow.weekday() != Weekday::Mon {
        let monday = now.date_naive() + Duration::days(i64::from(7 - now.weekday().num_days_from_monday()));
        out.push(("Monday morning".to_owned(), at(monday, 8, 0)));
    }
    out.into_iter().filter_map(|(label, at)| Some((format!("{label}  ·  {}", when_text(at?)), at?))).collect()
}

/// A day picked and a time typed ("8:30"), as a moment that is still to come.
pub fn parse_when(date: chrono::NaiveDate, time: &str, now: i64) -> Option<i64> {
    let time = chrono::NaiveTime::parse_from_str(time.trim(), "%H:%M").ok()?;
    let at = date.and_time(time).and_local_timezone(chrono::Local).earliest()?.timestamp();
    (at > now).then_some(at)
}

/// After Enter, with the caret at char `cursor` just past the new line: when the line before is a list
/// item or a quote, the next line starts the same way ("- ", "> ", the next number); when that item
/// was empty, it is taken away and the list ends there. The text and caret to use, or `None`.
pub fn continue_list(text: &str, cursor: usize) -> Option<(String, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if cursor == 0 || chars.get(cursor - 1) != Some(&'\n') {
        return None;
    }
    let end = cursor - 1;
    let start = chars[..end].iter().rposition(|c| *c == '\n').map_or(0, |p| p + 1);
    let line: String = chars[start..end].iter().collect();
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    let rest = &line[indent.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let (own, next) = if let Some(mark) = ["- ", "* ", "+ ", "> "].into_iter().find(|m| rest.starts_with(m)) {
        (mark.to_owned(), mark.to_owned())
    } else if !digits.is_empty() && rest[digits.len()..].starts_with(". ") {
        (format!("{digits}. "), format!("{}. ", digits.parse::<u64>().ok()?.checked_add(1)?))
    } else {
        return None;
    };
    let tail: String = chars[cursor..].iter().collect();
    let head: String = chars[..start].iter().collect();
    if rest.trim_end() == own.trim_end() {
        // An empty item: Enter ends the list and leaves an empty line where it was.
        return Some((format!("{head}{tail}"), start));
    }
    let added = format!("{indent}{next}");
    let before: String = chars[..cursor].iter().collect();
    Some((format!("{before}{added}{tail}"), cursor + added.chars().count()))
}

/// "Thu 16 Oct, 08:00", on this computer's clock.
pub fn when_text(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0).map(|t| t.with_timezone(&chrono::Local).format("%a %-d %b, %H:%M").to_string()).unwrap_or_default()
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
            bcc: String::new(),
            show_cc: false,
            show_bcc: false,
            subject: String::new(),
            body: String::new(),
            title: "New message",
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
            send_at: None,
            later_day: chrono::Local::now().date_naive() + chrono::Duration::days(1),
            later_month: chrono::Datelike::with_day(&(chrono::Local::now().date_naive() + chrono::Duration::days(1)), 1).unwrap_or_default(),
            later_time: "08:00".to_owned(),
            picking: false,
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
            sig: String::new(),
            sig_from: from,
            caret_top: false,
            contacts: Default::default(),
            suggesting: false,
            encrypt: false,
            encrypt_auto: true,
            can_encrypt: Err(String::new()),
            calendar: None,
        }
        .settled()
    }

    /// A message already addressed and written, such as the one a mailing list asks for to unsubscribe.
    pub fn addressed(from: u64, to: &str, subject: &str, body: &str) -> Self {
        Self { to: to.to_owned(), subject: subject.to_owned(), body: body.to_owned(), focus_to: false, ..Self::new(from) }.settled()
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
        (self.from, &self.to, &self.cc, &self.bcc, &self.subject, &self.body, self.title, &self.in_reply_to, &self.references).hash(&mut hasher);
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

    /// What the header says about the message: being sent, being copied to the server, or saved.
    /// Nothing while it is being written, so the header does not flicker at every key.
    fn status(&self) -> &'static str {
        match () {
            _ if self.sending && self.send_at.is_none() => "Sending…",
            _ if self.sending || self.is_empty() || self.unsaved() => "",
            _ if self.pushing => "Saving to the server…",
            _ if self.touched => "Draft saved",
            _ => "",
        }
    }

    /// The window's title: the subject once there is one, else what kind of message it is.
    fn heading(&self) -> &str {
        match self.subject.trim() {
            "" => self.title,
            subject => subject,
        }
    }

    pub fn to_draft(&self, updated: i64) -> DraftData {
        DraftData {
            id: self.id.clone(),
            title: self.title.to_owned(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: self.bcc.clone(),
            subject: self.subject.clone(),
            body: self.body.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            files: self.files.iter().filter(|f| f.ready).map(|f| DraftFile { name: f.name.clone(), size: f.size, path: f.vault.clone() }).collect(),
            updated,
            queued: false,
            send_at: None,
            encrypt: (!self.encrypt_auto).then_some(self.encrypt),
            calendar: self.calendar.clone(),
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
            bcc: draft.bcc,
            subject: draft.subject,
            body: draft.body,
            title,
            in_reply_to: draft.in_reply_to,
            references: draft.references,
            files: draft.files.into_iter().map(|f| AttachedFile { name: f.name, size: f.size, vault: f.path, ready: true }).collect(),
            encrypt: draft.encrypt.unwrap_or(false),
            encrypt_auto: draft.encrypt.is_none(),
            calendar: draft.calendar,
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

    /// Nothing typed yet (a signature does not count), so closing loses nothing.
    pub fn is_empty(&self) -> bool {
        [&self.to, &self.cc, &self.bcc, &self.subject, &self.body.replacen(&self.sig, "", 1)].iter().all(|t| t.trim().is_empty()) && self.files.is_empty()
    }

    /// The body starts with the signature of the account the message is sent from.
    /// Addressed to `to` (list items, the last finished with a separator), as an untouched draft.
    pub fn with_to(mut self, to: &str) -> Self {
        self.to = to.to_owned();
        self.settled()
    }

    pub fn with_signature(mut self, signature: &str) -> Self {
        self.sig = signature_block(signature);
        self.body.insert_str(0, &self.sig);
        self.caret_top = !self.sig.is_empty();
        self.settled()
    }

    /// The message is now sent from account `from`, whose signature is `signature`: the one in the body is
    /// replaced. shortcut: a body that has text but no signature to replace is left alone.
    pub fn change_signature(&mut self, from: u64, signature: &str) {
        let new = signature_block(signature);
        if !self.sig.is_empty() && self.body.contains(&self.sig) {
            self.body = self.body.replacen(&self.sig, &new, 1);
        } else if self.sig.is_empty() && self.body.trim().is_empty() {
            self.body = new.clone();
            self.caret_top = !new.is_empty();
        }
        self.sig = new;
        self.sig_from = from;
    }


    pub fn attached_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// What the sender needs to read the files: where in the vault, and the name the recipient sees.
    pub fn file_list(&self) -> Vec<(std::path::PathBuf, String)> {
        self.files.iter().map(|f| (f.vault.clone(), f.name.clone())).collect()
    }

    /// Every recipient's address (lower case), with whether it is a blind copy.
    pub fn recipients(&self) -> Vec<(String, bool)> {
        [(&self.to, false), (&self.cc, false), (&self.bcc, true)].into_iter().flat_map(|(list, blind)| addresses(list).into_iter().map(move |a| (address_of(&a), blind))).collect()
    }

    pub fn outgoing(&self, from_address: &str, rich: bool) -> Outgoing {
        Outgoing {
            from: from_address.to_owned(),
            to: addresses(&self.to),
            cc: addresses(&self.cc),
            bcc: addresses(&self.bcc),
            subject: self.subject.trim().to_owned(),
            body: self.body.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: Vec::new(), // read from `files` when the message is sent
            rich,
            autocrypt: None, // the app adds the keys, which it keeps
            seal: None,
            calendar: self.calendar.clone(),
        }
    }
}

/// "Encrypt", on when the message will go encrypted; off and saying why when it cannot.
fn encrypt_toggle(ui: &mut Ui, t: &Tokens, compose: &mut Compose) {
    let possible = compose.can_encrypt.is_ok();
    let on = compose.encrypt && possible;
    let tip = match &compose.can_encrypt {
        Ok(()) if on => "Encrypted: only the recipients can read it. Click to send it unencrypted".to_owned(),
        Ok(()) => "Every recipient's key is known: click to encrypt".to_owned(),
        Err(why) if why.is_empty() => "Add the recipients first".to_owned(),
        Err(why) => why.clone(),
    };
    let color = if on { t.accent } else { t.text_dim };
    let response = ui.add_enabled(possible, egui::Button::selectable(on, (Icon::Lock.image(14.0, color), RichText::new(if on { "Encrypted" } else { "Encrypt" }).color(color))).sense(NO_TAB));
    let response = if possible { response.on_hover_text(tip) } else { response.on_disabled_hover_text(tip) };
    if response.clicked() {
        compose.encrypt = !on;
        compose.encrypt_auto = false;
    }
}

/// A signature as it goes in the body: after a blank line and the "-- " line mail programs know it by.
fn signature_block(signature: &str) -> String {
    if signature.trim().is_empty() { String::new() } else { format!("\n\n-- \n{}", signature.trim_end()) }
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
    match message.address.trim() {
        "" => message.from.trim().to_owned(),
        address => beavermail_core::address_text(&message.from, address),
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
    split_addresses(text).0
}

/// The addresses of a list, and whether it ends with a separator (the last one is finished). A comma
/// or semicolon separates outside quotes, except inside a name written "Doe, John <john@x.org>": before
/// any address, with the `<address>` still to come before the next separator.
fn split_addresses(text: &str) -> (Vec<String>, bool) {
    let chars: Vec<char> = text.chars().collect();
    let (mut out, mut current, mut quoted) = (Vec::new(), String::new(), false);
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' | ';' | '\n' if !quoted => {
                let next: String = chars[i + 1..].iter().take_while(|c| !matches!(c, ',' | ';' | '\n')).collect();
                if !current.contains(['@', '<']) && !current.trim().is_empty() && next.contains('<') {
                    current.push(c); // a name with a comma in it
                } else {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    let finished = current.trim().is_empty();
    out.push(current);
    (out.into_iter().map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).collect(), finished)
}

/// The first address of the To, Cc and Bcc lists that cannot be sent to, for the reason Send is off.
fn first_invalid(compose: &Compose) -> Option<String> {
    [&compose.to, &compose.cc, &compose.bcc].into_iter().flat_map(|list| addresses(list)).find(|a| !beavermail_smtp::valid_address(a))
}

/// The address in a list item: what is inside `<>`, or the whole item; lower case.
fn address_of(item: &str) -> String {
    item.rsplit_once('<').map_or(item, |(_, rest)| rest.trim_end_matches('>')).trim().to_lowercase()
}

/// Up to six contacts, in their order, with a word of the name or address starting with each word
/// typed ("ada lo", "lovelace", "ada@ex", "nicolo" for Nicolò); those already in the list are left out.
fn suggestions<'a>(contacts: &'a [Contact], typed: &str, taken: &[String]) -> Vec<&'a Contact> {
    let taken: Vec<String> = taken.iter().map(|a| address_of(a)).collect();
    contacts.iter().filter(|c| c.matches(typed) && !taken.contains(&c.address)).take(6).collect()
}


/// An address field as chips, one per address finished (a comma, Enter, or leaving the field), red
/// when it cannot be sent to; what is being typed follows them, with the `contacts` it may mean below
/// (Up and Down choose, Enter or Tab or a click take one, Esc hides them). `text` stays the plain list.
/// Returns the field and whether suggestions are shown. The caller draws the divider under the row:
/// inside a row a separator is a vertical line that widens it.
fn recipients(ui: &mut Ui, t: &Tokens, id: egui::Id, text: &mut String, hint: &str, width: f32, contacts: &[Contact]) -> (egui::Response, bool) {
    let (mut done, finished) = split_addresses(text);
    let mut typing = if finished { String::new() } else { done.pop().unwrap_or_default() };
    let (mut remove, mut reopen) = (None, None);
    // What was offered last frame: before the field sees the keys, Up and Down move through it and Esc hides it.
    let (shown_id, pick_id, hidden_id) = (id.with("suggested"), id.with("pick"), id.with("hidden"));
    let was_shown = ui.data(|d| d.get_temp::<bool>(shown_id)).unwrap_or(false) && ui.memory(|m| m.has_focus(id));
    let (step, esc) = if was_shown {
        ui.input_mut(|i| (isize::from(i.consume_key(Modifiers::NONE, Key::ArrowDown)) - isize::from(i.consume_key(Modifiers::NONE, Key::ArrowUp)), i.consume_key(Modifiers::NONE, Key::Escape)))
    } else {
        (0, false)
    };
    if esc {
        ui.data_mut(|d| d.insert_temp(hidden_id, typing.clone()));
    }
    // Chips are as tall as their text, not as the theme's 40-point controls, so the row keeps its height;
    // set before the row is made, which takes its first line's height from it.
    let controls = ui.spacing().interact_size;
    ui.spacing_mut().interact_size.y = 0.0;
    // Top-aligned with a margin that centres one line of chips in the row: centring inside a wrapping
    // row places each item at its own height.
    let pad = ((ROW_H - CHIP_H) / 2.0) as i8;
    let shown = Frame::new().inner_margin(egui::Margin::symmetric(0, pad)).show(ui, |ui| {
        ui.set_width(width);
        ui.with_layout(Layout::left_to_right(Align::Min).with_main_wrap(true), |ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
            for (i, address) in done.iter().enumerate() {
                let valid = beavermail_smtp::valid_address(address);
                // "Ada <ada@x.org>" shows as Ada; the address is on hover.
                let name = address.split_once('<').map(|(n, _)| n.trim().trim_matches('"')).filter(|n| !n.is_empty()).unwrap_or(address);
                // A wrong one as an error badge: a faint red ground, red outline and red text, readable in
                // both themes (white on red would fall short of the contrast text needs).
                let (fill, stroke, color) = if valid { (t.hover, egui::Stroke::NONE, t.text) } else { (t.danger.gamma_multiply(0.16), egui::Stroke::new(1.0, t.danger), t.danger) };
                let margin = egui::Margin { left: 8, right: 3, top: 2, bottom: 2 };
                let chip = Frame::new().fill(fill).stroke(stroke).corner_radius(egui::CornerRadius::same(6)).inner_margin(margin).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // One line height for the icon, the name and the remove button, so they centre together.
                    ui.spacing_mut().interact_size.y = CHIP_H - 6.0;
                    ui.horizontal(|ui| {
                        if !valid {
                            ui.add(Icon::Spam.image(14.0, color));
                        }
                        // Clickable, but Tab passes it by: from one field to the next, not through every chip.
                        let label = ui.add(egui::Label::new(RichText::new(name).size(14.0).color(color)).sense(egui::Sense::CLICK));
                        let tip = if valid { address.clone() } else { format!("{address} is not an address mail can go to. Click to correct it.") };
                        if label.on_hover_text(tip).clicked() {
                            reopen = Some(i);
                        }
                        if crate::icons::button_small(ui, t, Icon::X, "Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                });
                chip.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, address));
            }
            // With an address typed, Tab finishes it as a chip and stays here; on an empty field it goes on.
            let holding = !typing.trim().is_empty();
            ui.add(
                TextEdit::singleline(&mut typing)
                    .id(id)
                    .lock_focus(holding)
                    .frame(Frame::NONE)
                    .hint_text(if done.is_empty() { hint } else { "" })
                    // What is left of the line, less the gap before it: `available_width` in a wrapping row is
                    // the whole width, which would always drop the field to a line of its own.
                    .desired_width((ui.available_size_before_wrap().x - ui.spacing().item_spacing.x).max(120.0))
                    // As tall as a chip: items of one height line up in a wrapping row.
                    .min_size(vec2(0.0, CHIP_H))
                    .vertical_align(Align::Center),
            )
        })
        .inner
    });
    ui.spacing_mut().interact_size = controls;
    let field = shown.inner;
    let (enter, backspace, tab) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Backspace), i.key_pressed(Key::Tab) && !i.modifiers.shift));
    // Also the frame the field lets go of the keys: a click on a suggestion takes them away from it.
    let here = field.has_focus() || field.lost_focus();
    let hidden = ui.data(|d| d.get_temp::<String>(hidden_id)).is_some_and(|h| h == typing);
    let found = if here && !hidden { suggestions(contacts, &typing, &done) } else { Vec::new() };
    let mut picked = None;
    if !found.is_empty() {
        let last = ui.data(|d| d.get_temp::<(String, usize)>(pick_id)).filter(|(typed, _)| *typed == typing).map_or(0, |(_, at)| at);
        let pick = last.saturating_add_signed(step).min(found.len() - 1);
        ui.data_mut(|d| d.insert_temp(pick_id, (typing.clone(), pick)));
        egui::Popup::from_response(&field).open(true).width(width.min(360.0)).show(|ui| {
            ui.spacing_mut().interact_size.y = 28.0; // a list, not a row of 40-point controls
            for (i, contact) in found.iter().enumerate() {
                let mut job = egui::text::LayoutJob::default();
                if !contact.name.is_empty() {
                    job.append(&contact.name, 0.0, egui::TextFormat::simple(egui::FontId::proportional(14.0), t.text));
                }
                job.append(&contact.address, if contact.name.is_empty() { 0.0 } else { 8.0 }, egui::TextFormat::simple(egui::FontId::proportional(13.0), t.text_dim));
                // Not focusable, so pressing it leaves the field the keys; taken on the press, before the field lets go.
                // One line each, as wide as the list: a long name or address is cut, not wrapped.
                let row = ui.add(egui::Button::selectable(i == pick, job).truncate().min_size(vec2(ui.available_width(), 0.0)).sense(egui::Sense::CLICK));
                if row.clicked() || row.is_pointer_button_down_on() {
                    picked = Some(i);
                }
            }
        });
        if (enter || tab) && here {
            picked = picked.or(Some(pick));
        }
    }
    ui.data_mut(|d| d.insert_temp(shown_id, !found.is_empty()));
    // With suggestions shown Esc is for them: egui would otherwise take the keys away from the field first.
    let filter = egui::EventFilter { tab: !typing.trim().is_empty(), horizontal_arrows: true, vertical_arrows: true, escape: !found.is_empty() };
    ui.memory_mut(|m| m.set_focus_lock_filter(id, filter));
    if let Some(i) = picked {
        typing = beavermail_core::address_text(&found[i].name, &found[i].address);
        done.push(std::mem::take(&mut typing));
        field.request_focus();
    }
    if let Some(i) = remove {
        done.remove(i);
    }
    if let Some(i) = reopen {
        // Back into the field to be corrected; what was being typed becomes a chip first.
        let address = done.remove(i);
        if !typing.trim().is_empty() {
            done.push(std::mem::take(&mut typing));
        }
        typing = address;
        field.request_focus();
    } else if field.has_focus() && backspace && typing.is_empty() && !field.changed() {
        typing = done.pop().unwrap_or_default();
    }
    let finish = (enter || tab) && field.has_focus();
    if !typing.trim().is_empty() && (field.lost_focus() || finish) {
        done.push(std::mem::take(&mut typing));
        if finish {
            field.request_focus(); // Enter and Tab finish the address, not the field
        }
    }
    *text = match (done.is_empty(), typing.is_empty()) {
        (true, _) => typing,
        (false, true) => format!("{}, ", done.join(", ")),
        (false, false) => format!("{}, {typing}", done.join(", ")),
    };
    (field, !found.is_empty())
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
                    .id(egui::Id::new(("compose_row", name)))
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
                "B" => RichText::new(label).family(theme::bold()),
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
    let invalid = first_invalid(compose);
    let anyone = [&compose.to, &compose.cc, &compose.bcc].iter().any(|l| !addresses(l).is_empty());
    let ready = !compose.sending && anyone && invalid.is_none() && !waiting_for_files;
    let why_not = match &invalid {
        _ if waiting_for_files => "Waiting for the attachments to be added".to_owned(),
        Some(address) => format!("{address} is not an address mail can go to"),
        None if !anyone => "Add a recipient first".to_owned(),
        None => String::new(),
    };
    let (contacts, mut suggesting) = (compose.contacts.clone(), false);
    let shown = egui::Window::new(compose.title)
        .id(egui::Id::new("compose"))
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
        .default_width(620.0)
        .resizable(true)
        .collapsible(false)
        .title_bar(false)
        .show(ctx, |ui| {
            // The header: what kind of message, its subject once written, and whether it is saved.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                let icon = match compose.title {
                    "Reply" => Icon::Reply,
                    "Reply all" => Icon::ReplyAll,
                    "Forward" => Icon::Forward,
                    _ => Icon::Pencil,
                };
                ui.add(icon.image(crate::icons::SIZE, t.accent));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if crate::icons::button(ui, t, Icon::X, "Close and keep it as a draft", !compose.sending, false).clicked() {
                        outcome = Outcome::Close;
                    }
                    let status = compose.status();
                    if !status.is_empty() {
                        ui.label(RichText::new(status).size(12.5).color(t.text_dim));
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(compose.heading()).size(theme::SUBTITLE).family(theme::bold()).color(t.text)).truncate());
                        if !compose.subject.trim().is_empty() {
                            ui.label(RichText::new(compose.title).size(12.5).color(t.text_dim));
                        }
                    });
                });
            });
            ui.separator();
            ui.add_enabled_ui(!compose.sending, |ui| {
                ui.horizontal(|ui| {
                    label(ui, t, "From");
                    let current = senders.iter().find(|(s, _)| *s == compose.from).map(|(_, a)| a.as_str()).unwrap_or("");
                    egui::ComboBox::from_id_salt("compose_from").selected_text(current).show_ui(ui, |ui| {
                        for (serial, address) in senders {
                            ui.selectable_value(&mut compose.from, *serial, address);
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| encrypt_toggle(ui, t, compose));
                });
                ui.separator();
                // Cc and Bcc are asked for beside To, or with their keys; one in use stays shown.
                for (key, show, salt) in [(CC_KEY, &mut compose.show_cc, "compose_cc"), (BCC_KEY, &mut compose.show_bcc, "compose_bcc")] {
                    if ctx.input_mut(|i| i.consume_shortcut(&key)) {
                        *show = true;
                        ctx.memory_mut(|m| m.request_focus(egui::Id::new(salt)));
                    }
                }
                let (show_cc, show_bcc) = (compose.show_cc || !compose.cc.trim().is_empty(), compose.show_bcc || !compose.bcc.trim().is_empty());
                let to = ui
                    .horizontal_top(|ui| {
                        label(ui, t, "To");
                        let links = if show_cc && show_bcc { 0.0 } else { 80.0 };
                        // The links' width and the gap before them: asking for more would widen the window each frame.
                        let gap = if links > 0.0 { ui.spacing().item_spacing.x } else { 0.0 };
                        let (field, offered) = recipients(ui, t, egui::Id::new("compose_to"), &mut compose.to, "name@example.com, …", ui.available_width() - links - gap, &contacts);
                        suggesting |= offered;
                        if links > 0.0 {
                            ui.allocate_ui_with_layout(vec2(links, ROW_H), Layout::right_to_left(Align::Center), |ui| {
                                // Out of the Tab order (To goes straight on to the next field); the keys open them.
                                let link = |ui: &mut Ui, text: &str, tip: &str| {
                                    ui.add(egui::Label::new(RichText::new(text).color(t.text_dim)).sense(egui::Sense::CLICK)).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
                                };
                                if !show_bcc && link(ui, "Bcc", &format!("Add blind copies: they get the message and nobody sees them ({})", ctx.format_shortcut(&BCC_KEY))) {
                                    compose.show_bcc = true;
                                }
                                if !show_cc && link(ui, "Cc", &format!("Add copies ({})", ctx.format_shortcut(&CC_KEY))) {
                                    compose.show_cc = true;
                                }
                            });
                        }
                        field
                    })
                    .inner;
                ui.separator();
                if std::mem::take(&mut compose.focus_to) {
                    to.request_focus();
                }
                for (shown, name, list, salt) in [(show_cc, "Cc", &mut compose.cc, "compose_cc"), (show_bcc, "Bcc", &mut compose.bcc, "compose_bcc")] {
                    if shown {
                        ui.horizontal_top(|ui| {
                            label(ui, t, name);
                            suggesting |= recipients(ui, t, egui::Id::new(salt), list, "", ui.available_width(), &contacts).1;
                        });
                        ui.separator();
                    }
                }
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
                    if std::mem::take(&mut compose.caret_top) {
                        let mut state = egui::text_edit::TextEditState::default();
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(0))));
                        state.store(ctx, id);
                    }
                    let length = compose.body.chars().count();
                    let mut output = TextEdit::multiline(&mut compose.body)
                        .id(id)
                        .frame(Frame::NONE)
                        .hint_text("Write your message")
                        .desired_width(f32::INFINITY)
                        .desired_rows(8)
                        .show(ui);
                    // One character typed, and it was a new line: carry a list or quote on to it.
                    if output.response.changed()
                        && compose.body.chars().count() == length + 1
                        && let Some(range) = output.cursor_range.filter(|r| r.is_empty())
                        && let Some((text, caret)) = continue_list(&compose.body, range.primary.index.0)
                    {
                        compose.body = text;
                        let mut state = output.state.clone();
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(caret))));
                        state.store(ctx, id);
                    }
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
                let send = if why_not.is_empty() { send } else { send.on_disabled_hover_text(&why_not) };
                if send.clicked() {
                    outcome = Outcome::Send;
                }
                ui.add_enabled_ui(ready, |ui| {
                    let look = egui::containers::menu::MenuConfig::new().style(views::menu_look);
                    let menu = egui::containers::menu::MenuButton::new("Send later").config(look).ui(ui, |ui| {
                        for (label, at) in later_choices(chrono::Local::now()) {
                            if ui.button(label).clicked() {
                                outcome = Outcome::Schedule(at);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Pick date & time…").clicked() {
                            compose.picking = true;
                            ui.close();
                        }
                    });
                    menu.0.on_hover_text("Choose when it is sent; BeaverMail sends it then, if it is open");
                });
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
                });
            });
        });
    if compose.picking
        && let Some(at) = pick_when(ctx, t, compose)
    {
        outcome = Outcome::Schedule(at);
    }
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
    compose.suggesting = suggesting;
    outcome
}

/// The width of the "Pick date & time" dialog: the month, a gap, and the date and time beside it.
const PICK_WIDTH: f32 = 460.0;

/// The "Pick date & time" dialog, as Gmail's: a month to pick the day from and the time in half
/// hours. The moment chosen when Schedule send is pressed; the dialog closes then or on Cancel and Esc.
fn pick_when(ctx: &egui::Context, t: &Tokens, compose: &mut Compose) -> Option<i64> {
    let now = chrono::Local::now();
    let (mut chosen, mut cancel) = (None, false);
    let modal = egui::Modal::new(egui::Id::new("pick_when")).show(ctx, |ui| {
        ui.set_width(PICK_WIDTH);
        ui.heading("Pick date & time");
        ui.label(RichText::new("BeaverMail sends it then, if it is open.").color(t.text_dim));
        ui.add_space(12.0);
        // As Gmail: the month on the left, the day picked and the time on the right.
        ui.horizontal_top(|ui| {
            // Its own column: inside the row it would lay its weeks side by side.
            ui.vertical(|ui| views::calendar(ui, t, &mut compose.later_day, &mut compose.later_month, now.date_naive(), Some(now.date_naive())));
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("Date").size(13.0).color(t.text_dim));
                ui.label(RichText::new(compose.later_day.format("%A %-d %B").to_string()).size(theme::SUBTITLE).family(theme::bold()));
                ui.add_space(16.0);
                ui.label(RichText::new("Time").size(13.0).color(t.text_dim));
                egui::ComboBox::from_id_salt("pick_time").width(140.0).selected_text(compose.later_time.clone()).show_ui(ui, |ui| {
                    for minutes in (0..24 * 60).step_by(30) {
                        let text = format!("{:02}:{:02}", minutes / 60, minutes % 60);
                        ui.selectable_value(&mut compose.later_time, text.clone(), text);
                    }
                });
            });
        });
        ui.add_space(16.0);
        let at = parse_when(compose.later_day, &compose.later_time, now.timestamp());
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let button = egui::Button::new(RichText::new("Schedule send").color(t.on_accent)).fill(t.accent);
            if ui.add_enabled(at.is_some(), button).on_disabled_hover_text("That moment has passed: pick a later one").clicked() {
                chosen = at;
            }
            cancel = ui.button("Cancel").clicked();
        });
    });
    if chosen.is_some() || cancel || modal.should_close() {
        compose.picking = false;
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pick_dialog_lays_the_month_out_as_weeks_beside_the_date_and_time() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))), ..Default::default() };
        let mut compose = Compose::new(1);
        compose.picking = true;
        // Two passes: the first one measures.
        for _ in 0..2 {
            let mut output = ctx.run_ui(input(), |ui| {
                pick_when(ui.ctx(), &Tokens::dark(), &mut compose);
            });
            output.textures_delta.clear(); // nothing is drawn here
        }
        let rect = ctx.memory(|m| m.area_rect(egui::Id::new("pick_when"))).expect("the dialog is shown");
        assert!(rect.width() < PICK_WIDTH + 80.0, "as wide as the dialog, not a row of every day: {rect:?}");
        assert!(rect.height() > 300.0 && rect.height() < 560.0, "weeks under each other: {rect:?}");
    }

    #[test]
    fn the_compose_window_keeps_its_size_with_chips_and_the_optional_rows() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))), ..Default::default() };
        let mut compose = Compose::new(1);
        compose.to = "Ada Lovelace <ada@x.org>, dasds, grace@y.org, bo".into();
        let size = |compose: &mut Compose| {
            for _ in 0..2 {
                let mut output = ctx.run_ui(input(), |ui| {
                    window(ui.ctx(), &Tokens::dark(), compose, &[(1, "me@x.org".into())], false);
                });
                output.textures_delta.clear(); // nothing is drawn here
            }
            ctx.memory(|m| m.area_rect(egui::Id::new("compose"))).expect("the window is shown").size()
        };
        size(&mut compose);
        let plain = size(&mut compose);
        assert!(plain.x < 900.0 && plain.y < 800.0, "chips wrap inside the window: {plain:?}");
        // Two short chips leave room for the field beside them: the row stays one line.
        let mut short = Compose::new(1);
        short.to = "ada@x.org, bob@y.org, ".into();
        size(&mut short);
        let chips = size(&mut short);
        let mut empty = Compose::new(1);
        size(&mut empty);
        assert_eq!(chips.y, size(&mut empty).y, "the field stays beside the chips, not on a line of its own");
        compose.show_cc = true;
        compose.show_bcc = true;
        size(&mut compose); // the new rows take a frame or two to be measured
        let more = size(&mut compose);
        assert!(more.y > plain.y && more.y < plain.y + 4.0 * ROW_H, "Cc and Bcc add a row each: {plain:?} then {more:?}");
        assert_eq!(size(&mut compose), more, "the window settles: frames after do not widen it");
    }

    #[test]
    fn tab_goes_from_to_to_the_next_field_not_through_chips_or_links() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut compose = Compose::new(1);
        compose.to = "ada@x.org, bob@y.org, ".into();
        let frame = |events: Vec<egui::Event>, compose: &mut Compose| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                window(ui.ctx(), &Tokens::dark(), compose, &[(1, "me@x.org".into())], false);
            });
            output.textures_delta.clear(); // nothing is drawn here
        };
        frame(Vec::new(), &mut compose);
        ctx.memory_mut(|m| m.request_focus(egui::Id::new("compose_to")));
        frame(Vec::new(), &mut compose);
        let tab = egui::Event::Key { key: Key::Tab, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        // An address being typed: Tab makes it a chip and stays for the next one.
        compose.to.push_str("carl@z.org");
        frame(Vec::new(), &mut compose);
        frame(vec![tab.clone()], &mut compose);
        frame(Vec::new(), &mut compose);
        assert_eq!(ctx.memory(|m| m.focused()), Some(egui::Id::new("compose_to")), "still in To");
        assert_eq!(addresses(&compose.to), ["ada@x.org", "bob@y.org", "carl@z.org"]);
        assert!(compose.to.ends_with(", "), "carl@z.org is a chip now: {:?}", compose.to);
        // Nothing typed: Tab goes on.
        frame(vec![tab], &mut compose);
        frame(Vec::new(), &mut compose);
        assert_eq!(ctx.memory(|m| m.focused()), Some(egui::Id::new(("compose_row", "Subject"))));
    }

    #[test]
    fn suggestions_match_the_start_of_any_word_and_skip_who_is_already_there() {
        let contacts = [Contact::new("Ada Lovelace", "ada@x.org"), Contact::new("", "adam@y.org"), Contact::new("Grace Hopper", "grace@navy.mil")];
        let found = |typed: &str, taken: &[String]| suggestions(&contacts, typed, taken).into_iter().map(|c| c.address.as_str()).collect::<Vec<_>>();
        assert_eq!(found("ada", &[]), ["ada@x.org", "adam@y.org"]);
        assert_eq!(found("love", &[]), ["ada@x.org"]);
        assert_eq!(found("ada lo", &[]), ["ada@x.org"], "every word typed");
        assert_eq!(found("navy", &[]), ["grace@navy.mil"]);
        assert_eq!(found("ada", &["Ada <ADA@x.org>".into()]), ["adam@y.org"], "already in the list");
        assert!(found("  ", &[]).is_empty() && found("zed", &[]).is_empty());
        assert_eq!(beavermail_core::address_text("Ada Lovelace", "ada@x.org"), "Ada Lovelace <ada@x.org>");
        assert_eq!(beavermail_core::address_text("Doe, John", "john@x.org"), "\"Doe, John\" <john@x.org>");
        assert_eq!(beavermail_core::address_text("", "adam@y.org"), "adam@y.org");
    }

    #[test]
    fn a_suggestion_is_taken_with_enter_or_a_click_and_esc_hides_them() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut compose = Compose::new(1);
        let long = "ada.lovelace.countess.of.lovelace.analytical.engine@a-very-long-domain-name.example.org";
        compose.contacts = std::sync::Arc::new(vec![Contact::new("Ada Augusta King, Countess of Lovelace", long), Contact::new("Adam Smith", "adam@y.org")]);
        let frame = |events: Vec<egui::Event>, compose: &mut Compose| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                window(ui.ctx(), &Tokens::dark(), compose, &[(1, "me@x.org".into())], false);
            });
            output.textures_delta.clear(); // nothing is drawn here
        };
        let key = |key: Key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame(Vec::new(), &mut compose);
        frame(vec![egui::Event::Text("ad".into())], &mut compose);
        frame(Vec::new(), &mut compose);
        assert!(compose.suggesting, "two people start with ad");
        // Down to the second, Enter takes it.
        frame(vec![key(Key::ArrowDown)], &mut compose);
        frame(vec![key(Key::Enter)], &mut compose);
        frame(Vec::new(), &mut compose);
        assert_eq!(compose.to, "Adam Smith <adam@y.org>, ");
        assert_eq!(ctx.memory(|m| m.focused()), Some(egui::Id::new("compose_to")), "still in To for the next one");
        // Esc hides them, and the window stays: the caller only discards when nothing is suggested.
        frame(vec![egui::Event::Text("lov".into())], &mut compose);
        frame(Vec::new(), &mut compose);
        assert!(compose.suggesting);
        frame(vec![key(Key::Escape)], &mut compose);
        frame(Vec::new(), &mut compose);
        assert!(!compose.suggesting, "hidden");
        assert!(compose.to.ends_with("lov"), "what was typed stays: {:?}", compose.to);
        // Typing on shows them again; a click takes one.
        frame(vec![egui::Event::Text("e".into())], &mut compose);
        frame(Vec::new(), &mut compose);
        let list = ctx.memory(|m| m.area_rect(egui::Id::new("compose_to").with("popup"))).expect("the suggestions are shown");
        assert!(list.height() < 60.0, "one person, one line (wrapped it was 100 tall): {list:?}");
        let at = list.center();
        frame(vec![egui::Event::PointerMoved(at)], &mut compose);
        frame(vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }], &mut compose);
        frame(vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }], &mut compose);
        frame(Vec::new(), &mut compose);
        assert_eq!(addresses(&compose.to), ["Adam Smith <adam@y.org>".to_owned(), format!("\"Ada Augusta King, Countess of Lovelace\" <{long}>")], "{:?}", compose.to);
    }

    #[test]
    fn addresses_split_on_separators_but_not_inside_a_name() {
        assert_eq!(split_addresses("ada@x.org, bob@y.org; carl@z.org"), (vec!["ada@x.org".into(), "bob@y.org".into(), "carl@z.org".into()], false));
        assert_eq!(addresses("Doe, John <john@x.org>, \"Lee, Ann\" <ann@y.org>"), ["Doe, John <john@x.org>", "\"Lee, Ann\" <ann@y.org>"]);
        assert_eq!(split_addresses("ada@x.org, "), (vec!["ada@x.org".into()], true), "a separator at the end: the last one is finished");
        assert_eq!(split_addresses("ada@x.org, bo"), (vec!["ada@x.org".into(), "bo".into()], false), "still being typed");
        assert_eq!(addresses("asdas,asdasd,adasdasdas"), ["asdas", "asdasd", "adasdasdas"], "a comma makes a chip, even of a wrong address");
        assert_eq!(split_addresses("Doe,"), (vec!["Doe".into()], true), "typed so far: a chip, until the rest of the name comes");
        assert!(split_addresses("").0.is_empty());
    }

    #[test]
    fn send_names_the_first_address_it_cannot_go_to() {
        let mut c = Compose::new(1);
        c.to = "ada@x.org, dasds".into();
        assert_eq!(first_invalid(&c), Some("dasds".into()));
        c.to = "ada@x.org".into();
        c.bcc = "hidden@y.org".into();
        assert_eq!(first_invalid(&c), None);
        assert_eq!(c.outgoing("me@x.org", false).bcc, ["hidden@y.org"]);
    }

    #[test]
    fn enter_carries_a_list_on_and_ends_it_on_an_empty_item() {
        let after_enter = |text: &str| continue_list(text, text.chars().count());
        assert_eq!(after_enter("- milk\n"), Some(("- milk\n- ".into(), 9)));
        assert_eq!(after_enter("  * eggs\n"), Some(("  * eggs\n  * ".into(), 13)), "the indent stays");
        assert_eq!(after_enter("9. bread\n"), Some(("9. bread\n10. ".into(), 13)), "the next number");
        assert_eq!(after_enter("> quoted\n"), Some(("> quoted\n> ".into(), 11)));
        assert_eq!(after_enter("- milk\n- \n"), Some(("- milk\n".into(), 7)), "an empty item ends the list");
        assert_eq!(after_enter("plain text\n"), None);
        assert_eq!(after_enter("-not a list\n"), None);
        assert_eq!(continue_list("- a\nb", 4), Some(("- a\n- b".into(), 6)), "Enter inside an item splits it in two items");
        assert_eq!(continue_list("- a\nb", 5), None, "only right after the new line");
    }

    #[test]
    fn the_header_names_the_subject_and_says_when_the_draft_is_saved() {
        let mut c = Compose::new(1);
        assert_eq!((c.heading(), c.status()), ("New message", ""), "nothing written, nothing to say");
        c.subject = "  Lunch on Friday ".into();
        assert_eq!(c.heading(), "Lunch on Friday");
        assert_eq!(c.status(), "", "written but not saved yet: quiet");
        c.mark_saved();
        assert_eq!(c.status(), "Draft saved");
        c.pushing = true;
        assert_eq!(c.status(), "Saving to the server…");
        c.sending = true;
        assert_eq!(c.status(), "Sending…");
    }

    #[test]
    fn send_later_offers_moments_to_come_and_reads_a_typed_one() {
        use chrono::TimeZone;
        let labels = |now| later_choices(now).into_iter().map(|(l, _)| l.split("  ·").next().unwrap_or_default().to_owned()).collect::<Vec<_>>();
        // A Sunday afternoon: tomorrow is Monday, so "Monday morning" is not offered twice.
        let sunday = chrono::Local.with_ymd_and_hms(2026, 10, 18, 15, 0, 0).unwrap();
        assert_eq!(labels(sunday), ["Later today", "Tomorrow morning", "Tomorrow afternoon"]);
        // A Thursday morning: later today, this evening, and the days after.
        let morning = chrono::Local.with_ymd_and_hms(2026, 10, 15, 9, 10, 0).unwrap();
        assert_eq!(labels(morning), ["Later today", "This evening", "Tomorrow morning", "Tomorrow afternoon", "Monday morning"]);
        let choices = later_choices(morning);
        assert_eq!(when_text(choices[0].1), "Thu 15 Oct, 12:30", "three hours on, on the next half hour");
        assert!(choices.iter().all(|(_, at)| *at > morning.timestamp()));
        assert_eq!(when_text(choices[4].1), "Mon 19 Oct, 08:00");
        // Late at night there is nothing left of today.
        let night = chrono::Local.with_ymd_and_hms(2026, 10, 15, 22, 0, 0).unwrap();
        assert_eq!(labels(night)[0], "Tomorrow morning");
        let thursday = morning;
        let now = thursday.timestamp();
        let day = |d: &str| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap();
        assert!(parse_when(day("2026-10-16"), "8:30", now).is_some());
        assert_eq!(parse_when(day("2026-10-14"), "08:00", now), None, "a moment gone by");
        assert_eq!(parse_when(day("2026-10-16"), "8h30", now), None);
    }

    #[test]
    fn a_signature_goes_under_the_text_and_is_not_text() {
        let mut c = Compose::new(1).with_signature("Ada\nAcme\n");
        assert_eq!(c.body, "\n\n-- \nAda\nAcme");
        assert!(c.is_empty(), "only a signature: nothing to lose");
        c.change_signature(2, "Other");
        assert_eq!((c.body.as_str(), c.sig_from), ("\n\n-- \nOther", 2), "another account, its signature");
        c.change_signature(3, "");
        assert_eq!(c.body, "", "an account without one");
        let mut c = Compose::new(1).with_signature("A");
        c.body.insert_str(0, "hello");
        c.change_signature(2, "B");
        assert_eq!(c.body, "hello\n\n-- \nB", "what was typed stays above");
        assert!(!c.is_empty());
        let mut c = Compose::new(1);
        c.change_signature(2, "B");
        assert_eq!(c.body, "\n\n-- \nB", "an empty body takes the new account's signature");
        let mut c = Compose::new(1);
        c.body = "hi".into();
        c.change_signature(2, "B");
        assert_eq!(c.body, "hi", "text without a signature to replace is left alone");
    }

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
            parents: Vec::new(),
            unread: false,
            pinned: false,
            starred: false,
            priority: 0,
            labels: Vec::new(),
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
