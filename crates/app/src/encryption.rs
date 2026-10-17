// SPDX-License-Identifier: AGPL-3.0-only

//! Settings > Encryption: each account's own key, made here or imported, and its backup.

use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::theme::{self, Tokens};
use crate::views;

/// What the user asked for on the page or in its dialogs.
pub enum KeyAction {
    /// Make a key for this address.
    TurnOn(String),
    /// Pick a key file to import for this address.
    Import(String),
    /// Unlock the picked file with this password.
    Unlock(String, String),
    /// Show the backup dialog for this address's key.
    AskBackup(String),
    /// Save a backup of this address's key, locked with this code.
    SaveBackup(String, String),
    /// Ask before forgetting this address's key, or (`true`) its certificate.
    AskRemove(String, bool),
    /// Forget this address's key, or (`true`) its certificate.
    Remove(String, bool),
}

/// What one account address has: its OpenPGP key (short id) and its S/MIME certificate.
pub struct Keys {
    pub address: String,
    pub openpgp: Option<String>,
    pub smime: Option<beavermail_crypto::CertificateInfo>,
}

/// The dialogs the page may have open.
#[derive(Default)]
pub struct Dialogs {
    /// Back up a key.
    pub backup: Option<Backup>,
    /// A picked file needs its password: the address, the file, what is typed and why the last try failed.
    pub unlock: Option<Unlock>,
    /// Confirm forgetting this address's key, or (`true`) its certificate.
    pub remove: Option<(String, bool)>,
}

/// The backup dialog: a code made here, or a password the user chooses instead.
pub struct Backup {
    pub address: String,
    pub code: String,
    /// The user chose their own password: typed twice.
    pub own: bool,
    pub password: String,
    pub repeat: String,
}

impl Backup {
    pub fn new(address: String) -> Self {
        Self { address, code: beavermail_crypto::backup_password(), own: false, password: String::new(), repeat: String::new() }
    }

    /// What the file is locked with, or why the user's own password cannot be used yet.
    fn lock(&self) -> Result<&str, &'static str> {
        if !self.own {
            return Ok(&self.code);
        }
        match (self.password.chars().count(), self.password == self.repeat) {
            (0, _) => Err(""),
            (1..12, _) => Err("At least 12 characters: the file may end up anywhere, and this is all that protects it."),
            (_, false) => Err("The two passwords differ."),
            _ => Ok(&self.password),
        }
    }
}

pub struct Unlock {
    pub address: String,
    pub file: Vec<u8>,
    /// The file is an S/MIME certificate, not an OpenPGP key.
    pub certificate: bool,
    pub password: String,
    pub error: Option<String>,
}

/// The page: for each account address, its key and certificate, or the way to get them.
pub fn page(ui: &mut egui::Ui, t: &Tokens, accounts: &[Keys]) -> Option<KeyAction> {
    let hint = |ui: &mut egui::Ui, text: &str| ui.add(egui::Label::new(RichText::new(text).size(13.0).color(t.text_dim)).wrap());
    let mut action = None;
    hint(
        ui,
        "With a key, mail encrypted to you opens here, and you can encrypt to people whose key you have. Your key goes out with each message you send, so their programs (Thunderbird, Delta Chat and others) learn it and can write to you encrypted. Nothing is uploaded anywhere.",
    );
    for keys in accounts {
        let address = &keys.address;
        crate::section_title(ui, address);
        match keys.openpgp.clone() {
            None => {
                hint(ui, if keys.smime.is_some() { "No OpenPGP key: turn it on to also write encrypted to people who use OpenPGP." } else { "Encryption is off for this address." });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if views::primary_button(ui, t, "Turn on encryption", true).on_hover_text("Make a key for this address, on this computer").clicked() {
                        action = Some(KeyAction::TurnOn(address.clone()));
                    }
                    if ui.button("Import a key…").on_hover_text("An OpenPGP key (.asc) from a backup or another program, or an S/MIME certificate (.p12)").clicked() {
                        action = Some(KeyAction::Import(address.clone()));
                    }
                });
            }
            Some(id) => {
                ui.horizontal(|ui| {
                    ui.add(crate::icons::Icon::Key.image(crate::icons::SIZE, t.accent));
                    ui.label(RichText::new(format!("On  ·  your key {id}")).color(t.text));
                });
                hint(ui, "Your key is only on this computer. Back it up to keep reading encrypted mail after a reinstall or on another device.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Back up key…").clicked() {
                        action = Some(KeyAction::AskBackup(address.clone()));
                    }
                    if ui.button(RichText::new("Remove key…").color(t.danger)).clicked() {
                        action = Some(KeyAction::AskRemove(address.clone(), false));
                    }
                });
            }
        }
        // S/MIME only from a certificate an authority issued: imported, never made here.
        if let Some(info) = &keys.smime {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add(crate::icons::Icon::Lock.image(crate::icons::SIZE, t.accent));
                ui.label(RichText::new(format!("S/MIME certificate from {}, valid until {}", info.issuer, info.expires)).color(t.text));
            });
            if !info.address.eq_ignore_ascii_case(address) {
                ui.colored_label(t.danger, format!("It is for {}, not this address: mail signed with it shows as signed by someone else.", info.address));
            }
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Remove certificate…").color(t.danger)).clicked() {
                    action = Some(KeyAction::AskRemove(address.clone(), true));
                }
            });
        } else if keys.openpgp.is_some() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                hint(ui, "Have an S/MIME certificate from work?");
                if ui.button("Import certificate…").clicked() {
                    action = Some(KeyAction::Import(address.clone()));
                }
            });
        }
    }
    action
}

/// The open dialog, if any; returns what was confirmed.
pub fn dialogs(ctx: &egui::Context, t: &Tokens, dialogs: &mut Dialogs) -> Option<KeyAction> {
    let mut action = None;
    let dim = |ui: &mut egui::Ui, text: &str| ui.add(egui::Label::new(RichText::new(text).color(t.text_dim)).wrap());
    let buttons = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), add);
    };
    if let Some(backup) = dialogs.backup.as_mut() {
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("key_backup")).show(ctx, |ui| {
            ui.set_width(crate::FORM_WIDTH);
            ui.heading("Back up your key");
            let what = if backup.own { "the password you choose" } else { "this code" };
            dim(ui, &format!("A file with the key of {}, to read your encrypted mail on another device or after a reinstall. It opens only with {what}: keep it apart from the file if you can. BeaverMail does not keep it.", backup.address));
            ui.add_space(12.0);
            if backup.own {
                crate::form_grid(ui, "backup_password", |ui| {
                    ui.label("Password");
                    ui.add(crate::field(&mut backup.password).password(true).hint_text("At least 12 characters"));
                    ui.end_row();
                    ui.label("Repeat");
                    ui.add(crate::field(&mut backup.repeat).password(true));
                    ui.end_row();
                });
                // The length is said as soon as typing starts; a mismatch only once the second field is typed.
                if let Err(why) = backup.lock()
                    && !why.is_empty()
                    && (!why.starts_with("The two") || !backup.repeat.is_empty())
                {
                    ui.colored_label(t.danger, why);
                }
                if ui.link("Use a code made for me instead").clicked() {
                    backup.own = false;
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&backup.code).size(theme::SUBTITLE).family(theme::bold()).color(t.text));
                    if ui.button("Copy code").clicked() {
                        ui.ctx().copy_text(backup.code.clone());
                    }
                });
                ui.add_space(4.0);
                dim(ui, "Without the code the file cannot be opened, by you or anyone else.");
                if ui.link("Use my own password instead").clicked() {
                    backup.own = true;
                }
            }
            ui.add_space(12.0);
            let lock = backup.lock().map(str::to_owned);
            buttons(ui, &mut |ui| {
                let save = views::primary_button(ui, t, "Save backup file…", lock.is_ok()).on_disabled_hover_text("Type the password twice, at least 12 characters");
                if let (true, Ok(lock)) = (save.clicked(), &lock) {
                    action = Some(KeyAction::SaveBackup(backup.address.clone(), lock.clone()));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            dialogs.backup = None;
        }
    }
    if let Some(unlock) = dialogs.unlock.as_mut() {
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("key_unlock")).show(ctx, |ui| {
            ui.set_width(crate::FORM_WIDTH);
            ui.heading("Unlock the key");
            dim(ui, &format!("The file is protected. Type the password or code it was saved with, to use it for {}.", unlock.address));
            ui.add_space(12.0);
            let field = ui.add(crate::field(&mut unlock.password).password(true).hint_text("Password or backup code"));
            field.request_focus();
            if let Some(error) = &unlock.error {
                ui.colored_label(t.danger, error);
            }
            ui.add_space(12.0);
            let ready = !unlock.password.is_empty();
            buttons(ui, &mut |ui| {
                let enter = ready && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(ready, egui::Button::new(RichText::new("Unlock").color(t.on_accent)).fill(t.accent)).clicked() || enter {
                    action = Some(KeyAction::Unlock(unlock.address.clone(), unlock.password.clone()));
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            dialogs.unlock = None;
        }
    }
    if let Some((address, certificate)) = dialogs.remove.clone() {
        let mut close = false;
        let what = if certificate { "certificate" } else { "key" };
        let modal = egui::Modal::new(egui::Id::new("key_remove")).show(ctx, |ui| {
            ui.set_width(crate::FORM_WIDTH * 0.85);
            ui.heading(format!("Remove the {what} of {address}?"));
            dim(ui, &format!("Mail encrypted to it can no longer be read on this computer, until you import the {what} again from a backup. People who have it keep sending encrypted mail to it."));
            ui.add_space(12.0);
            buttons(ui, &mut |ui| {
                if ui.add(egui::Button::new(RichText::new(format!("Remove {what}")).color(t.on_accent)).fill(t.danger)).clicked() {
                    action = Some(KeyAction::Remove(address.clone(), certificate));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            dialogs.remove = None;
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_own_backup_password_needs_twelve_characters_typed_twice() {
        let mut backup = Backup::new("ada@example.org".into());
        assert_eq!(backup.lock().unwrap().len(), 44, "the code made here");
        backup.own = true;
        assert_eq!(backup.lock(), Err(""), "nothing typed yet: nothing to say");
        backup.password = "short".into();
        assert!(backup.lock().unwrap_err().starts_with("At least 12"));
        backup.password = "correct horse battery".into();
        assert_eq!(backup.lock(), Err("The two passwords differ."));
        backup.repeat = "correct horse battery".into();
        assert_eq!(backup.lock(), Ok("correct horse battery"));
    }

    #[test]
    fn the_page_and_the_backup_dialog_lay_out() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let accounts = [
            Keys { address: "ada@example.org".into(), openpgp: None, smime: None },
            Keys {
                address: "work@example.org".into(),
                openpgp: Some("1AD8 7E58 A740 1C0E".into()),
                smime: Some(beavermail_crypto::CertificateInfo { address: "work@example.org".into(), issuer: "Example CA".into(), expires: "2027-10-17".into() }),
            },
        ];
        let mut dialogs = Dialogs { backup: Some(Backup::new("work@example.org".into())), ..Default::default() };
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 900.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                assert!(page(ui, &Tokens::dark(), &accounts).is_none(), "nothing pressed");
                assert!(self::dialogs(ui.ctx(), &Tokens::dark(), &mut dialogs).is_none());
            });
            output.textures_delta.clear();
        }
        let shown = ctx.memory(|m| m.area_rect(egui::Id::new("key_backup"))).expect("the backup dialog is shown");
        assert!(shown.width() < 600.0 && shown.height() < 500.0, "{shown:?}");
    }
}
