//! Settings and prompts for how stored mail is protected: off, a key kept by the system,
//! or a passphrase.

use std::sync::atomic::{AtomicUsize, Ordering};

use beavermail_vault::{MIN_PASSPHRASE, Mode};
use eframe::egui::{self, Align, Layout, RichText, Ui, vec2};

use crate::theme::{self, Tokens};

/// Progress of a mode change that is rewriting files in the background.
#[derive(Default)]
pub struct Job {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
}

/// What the user asked for.
pub enum Request {
    Apply(Mode, Option<String>),
    Unlock(String),
    /// Forgot the passphrase: delete the stored mail and start over.
    Reset,
    /// Close the unlock prompt and carry on without stored mail.
    Skip,
}

#[derive(Default)]
pub struct Form {
    /// The mode picked in settings; set to the current one when settings open.
    pub mode: Option<Mode>,
    passphrase: String,
    repeat: String,
    unlock: String,
    pub unlock_error: Option<String>,
    confirm_reset: bool,
}

impl Form {
    pub fn clear_passphrases(&mut self) {
        self.passphrase.clear();
        self.repeat.clear();
    }

    pub fn clear_unlock(&mut self) {
        self.unlock.clear();
        self.unlock_error = None;
        self.confirm_reset = false;
    }
}

fn passphrase_field(ui: &mut Ui, text: &mut String, hint: &str) -> egui::Response {
    ui.add(egui::TextEdit::singleline(text).password(true).hint_text(hint).desired_width(f32::INFINITY).min_size(vec2(0.0, theme::CONTROL_H)).margin(egui::Margin::symmetric(12, 0)).vertical_align(Align::Center))
}

/// The "Stored mail" part of the settings window.
pub fn section(ui: &mut Ui, t: &Tokens, form: &mut Form, current: Mode, locked: bool, job: Option<&Job>) -> Option<Request> {
    if let Some(job) = job {
        let (done, total) = (job.done.load(Ordering::Relaxed), job.total.load(Ordering::Relaxed).max(1));
        ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(format!("Updating stored mail… {done} of {total}")));
        return None;
    }
    if locked {
        ui.colored_label(t.danger, "Stored mail is locked. Unlock it to change these settings.");
        return None;
    }
    let chosen = form.mode.get_or_insert(current);
    let options = [
        (Mode::System, "Encrypted, unlocked by this computer", "The key is kept in the system's credential store, so the app opens without asking. Protects a lost or copied disk."),
        (Mode::Passphrase, "Encrypted with my passphrase", "You type the passphrase at every start. If you forget it the stored mail cannot be opened; it can be downloaded again from the server."),
        (Mode::Off, "Not encrypted", "Messages are plain files that any program on this computer can read. Easy to back up and search with other tools."),
    ];
    for (mode, title, hint) in options {
        if ui.radio(*chosen == mode, title).clicked() {
            *chosen = mode;
        }
        ui.label(RichText::new(hint).size(13.0).color(t.text_dim));
        ui.add_space(4.0);
    }
    let chosen = *chosen;
    let mut request = None;
    if chosen == Mode::Off {
        ui.colored_label(t.danger, "Anyone with access to this computer's disk or its backups can read your mail.");
    }
    let mut valid = chosen != current;
    if chosen == Mode::Passphrase {
        ui.add_space(4.0);
        let label = if current == Mode::Passphrase { "New passphrase" } else { "Passphrase" };
        passphrase_field(ui, &mut form.passphrase, label);
        passphrase_field(ui, &mut form.repeat, "Repeat the passphrase");
        let problem = if form.passphrase.is_empty() {
            Some(format!("At least {MIN_PASSPHRASE} characters."))
        } else if form.passphrase.chars().count() < MIN_PASSPHRASE {
            Some(format!("Too short: at least {MIN_PASSPHRASE} characters."))
        } else if form.passphrase != form.repeat {
            Some("The two passphrases differ.".to_owned())
        } else {
            None
        };
        if let Some(problem) = &problem {
            ui.label(RichText::new(problem).size(13.0).color(t.text_dim));
        }
        valid = problem.is_none();
    }
    ui.add_space(6.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
        if ui.add_enabled(valid, egui::Button::new(RichText::new("Apply").color(t.on_accent)).fill(t.accent)).clicked() {
            let passphrase = (chosen == Mode::Passphrase).then(|| form.passphrase.clone());
            request = Some(Request::Apply(chosen, passphrase));
        }
    });
    request
}

/// The prompt shown at start when the stored mail is protected by a passphrase.
pub fn unlock_prompt(ctx: &egui::Context, t: &Tokens, form: &mut Form, busy: bool) -> Option<Request> {
    let mut request = None;
    let modal = egui::Modal::new(egui::Id::new("unlock_vault")).show(ctx, |ui| {
        ui.set_width(380.0);
        if form.confirm_reset {
            ui.heading("Delete stored mail?");
            ui.label(RichText::new("This removes every message stored on this device and starts again with a key kept by the system. Your mail is still on the server and is downloaded again.").color(t.text_dim));
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Delete stored mail").color(t.on_accent)).fill(t.danger)).clicked() {
                    request = Some(Request::Reset);
                }
                if ui.button("Back").clicked() {
                    form.confirm_reset = false;
                }
            });
            return;
        }
        ui.heading("Unlock stored mail");
        ui.label(RichText::new("Enter your passphrase to read mail stored on this device.").color(t.text_dim));
        ui.add_space(8.0);
        let field = passphrase_field(ui, &mut form.unlock, "Passphrase");
        if !field.has_focus() && form.unlock.is_empty() {
            field.request_focus();
        }
        if let Some(error) = &form.unlock_error {
            ui.add_space(4.0);
            ui.colored_label(t.danger, error);
        }
        ui.add_space(12.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let ready = !form.unlock.is_empty() && !busy;
            let unlock = ui.add_enabled(ready, egui::Button::new(RichText::new(if busy { "Unlocking…" } else { "Unlock" }).color(t.on_accent)).fill(t.accent));
            if unlock.clicked() || (ready && field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                request = Some(Request::Unlock(form.unlock.clone()));
            }
            if ui.button("Not now").clicked() {
                request = Some(Request::Skip);
            }
            if ui.button("Forgot passphrase…").clicked() {
                form.confirm_reset = true;
            }
        });
    });
    if modal.should_close() && !form.confirm_reset {
        request = request.or(Some(Request::Skip));
    }
    request
}
