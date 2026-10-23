// SPDX-License-Identifier: AGPL-3.0-only

//! Address books synced over CardDAV, each a source of its own: adding one (a server, a login, and
//! which of its books) and the list of those added, in the Contacts section.

use beavermail_core::AddressBook;
use eframe::egui::{self, Align, Layout, RichText, vec2};

use crate::theme::{self, Tokens};
use crate::views;

use beavermail_carddav::Found;

/// The "Add address book" dialog.
#[derive(Default)]
pub struct AddBook {
    pub server: String,
    pub user: String,
    pub password: String,
    /// The server is being asked for its address books.
    pub finding: bool,
    /// Focus the first field, once, as the dialog opens.
    pub focus_first: bool,
    /// What it answered: each address book, turned on to add or not, or why none.
    pub found: Option<Result<Vec<(Found, bool)>, String>>,
}

pub enum BookAction {
    /// Open the dialog to add one.
    Open,
    /// Show the contacts of this address book only (by URL), or of every one.
    Show(Option<String>),
    /// Ask the server in the dialog for its address books.
    Find,
    /// Sync these address books from now on, with the dialog's login.
    Add(Vec<Found>),
    /// New contacts go to this one.
    MakeDefault(String),
    /// Ask before no longer syncing this one.
    AskRemove(String),
    /// Sync every address book now.
    SyncNow,
    /// Fold or unfold a server's address books (by its key in the folded set).
    Fold(String),
}

/// The host of a URL, to name a server.
pub fn host(url: &str) -> &str {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url)
}

/// How an address book is called: its name, with its server when another one has the same name
/// (two "Contacts" on two servers).
pub fn label(book: &AddressBook, books: &[AddressBook]) -> String {
    let name = if book.name.is_empty() { host(&book.url) } else { book.name.as_str() };
    let twin = books.iter().any(|b| b.url != book.url && b.name == book.name);
    if twin && !book.name.is_empty() { format!("{name} ({})", host(&book.url)) } else { name.to_owned() }
}

/// The server an address book is on, as the Mail column names an account: its host, with the login
/// when the same host is there with another one too.
pub fn server(book: &AddressBook, books: &[AddressBook]) -> String {
    let other_login = books.iter().any(|b| host(&b.url) == host(&book.url) && b.user != book.user);
    if other_login { format!("{}@{}", book.user, host(&book.url)) } else { host(&book.url).to_owned() }
}

/// "All contacts", then the address books under the server they are on, as the Mail column lists the
/// folders under each account: click one to see its contacts only; right-click to make it the
/// default, sync it, or stop syncing it. `counts`: how many contacts each has (by URL), and all of
/// them; `folded`: the servers folded (by the key of their first book's password).
#[allow(clippy::too_many_arguments)]
pub fn section(ui: &mut egui::Ui, t: &Tokens, books: &[AddressBook], syncing: &std::collections::HashSet<String>, shown: Option<&str>, counts: &std::collections::HashMap<String, u32>, all: u32, folded: &std::collections::BTreeSet<String>) -> Option<BookAction> {
    let mut action = None;
    if views::icon_row(ui, t, "All contacts", crate::icons::Icon::Users, all, shown.is_none()).clicked() {
        action = Some(BookAction::Show(None));
    }
    if books.is_empty() {
        return action;
    }
    ui.add_space(8.0);
    ui.label(RichText::new("ADDRESS BOOKS").size(12.0).color(t.text_dim));
    let mut servers: Vec<String> = Vec::new();
    for book in books {
        let name = server(book, books);
        if !servers.contains(&name) {
            servers.push(name);
        }
    }
    for name in servers {
        let key = format!("carddav:{name}");
        let header = egui::CollapsingHeader::new(RichText::new(&name).size(15.0).color(t.text)).id_salt(&key).default_open(!folded.contains(&key)).icon(crate::views::fold_arrow).show(ui, |ui| {
            for book in books.iter().filter(|b| server(b, books) == name) {
                let name = if book.name.is_empty() { "Contacts" } else { book.name.as_str() };
                let title = match (book.default && books.len() > 1, syncing.contains(&book.url)) {
                    (_, true) => format!("{name}  ·  syncing"),
                    (true, false) => format!("{name}  ·  default"),
                    (false, false) => name.to_owned(),
                };
                let mut tip = format!("{} on {}", book.user, host(&book.url));
                if book.default {
                    tip.push_str("\nNew contacts go here");
                }
                let here = shown == Some(book.url.as_str());
                let row = views::icon_row(ui, t, &title, crate::icons::Icon::Users, counts.get(&book.url).copied().unwrap_or(0), here).on_hover_text(tip);
                if row.clicked() && !here {
                    action = Some(BookAction::Show(Some(book.url.clone())));
                }
                crate::views::context_menu(&row, |ui| {
                    let why = if book.read_only { "Read-only: the server makes it" } else { "New contacts already go here" };
                    if ui.add_enabled(!book.default && !book.read_only, egui::Button::new("Make default")).on_disabled_hover_text(why).clicked() {
                        action = Some(BookAction::MakeDefault(book.url.clone()));
                        ui.close();
                    }
                    if ui.button("Sync now").clicked() {
                        action = Some(BookAction::SyncNow);
                        ui.close();
                    }
                    if ui.button(RichText::new("Stop syncing…").color(t.danger)).clicked() {
                        action = Some(BookAction::AskRemove(book.url.clone()));
                        ui.close();
                    }
                });
            }
        });
        if header.header_response.clicked() {
            action = Some(BookAction::Fold(key));
        }
    }
    action
}

/// How the add dialog speaks of what it adds: address books, or calendars.
pub struct Words {
    pub id: &'static str,
    /// "address book", "calendar".
    pub one: &'static str,
    pub many: &'static str,
    pub about: &'static str,
    /// Beside one the server makes itself, read-only.
    pub read_only: &'static str,
}

pub const BOOKS: Words = Words { id: "add_book", one: "address book", many: "address books", about: "Sync your contacts with a CardDAV server.", read_only: "Read-only, made by the server: not your own contacts" };

/// The dialog to add address books or calendars: the server and login, then, when it has more than
/// one, which to add. One primary action: find them, then add those ticked. `added`: the URLs of
/// those already added.
pub fn add_dialog(ctx: &egui::Context, t: &Tokens, state: &mut Option<AddBook>, added: &[&str], words: &Words) -> Option<BookAction> {
    let form = state.as_mut()?;
    let mut action = None;
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new(words.id)).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        ui.heading(format!("Add {}", words.one));
        ui.label(RichText::new(words.about).color(t.text_dim));
        ui.add_space(12.0);
        let mut edited = false;
        crate::form_grid(ui, "add_book_form", |ui| {
            ui.label("Server");
            let server = ui.add(crate::field(&mut form.server).hint_text("dav.example.com, or example.com"));
            if std::mem::take(&mut form.focus_first) {
                server.request_focus();
            }
            ui.end_row();
            ui.label("User");
            let user = ui.add(crate::field(&mut form.user).hint_text("Usually your email address"));
            ui.end_row();
            ui.label("Password");
            let password = ui.add(crate::field(&mut form.password).password(true).hint_text("Often an app password"));
            ui.end_row();
            edited = server.changed() || user.changed() || password.changed();
        });
        // Another server or login: what was found belongs to the old one.
        if edited && !form.finding {
            form.found = None;
        }
        match &mut form.found {
            Some(Ok(found)) => {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("{} on this server", capital(words.many))).strong());
                for (book, on) in found.iter_mut() {
                    let already = added.contains(&book.url.as_str());
                    let label = if book.name.is_empty() { book.url.clone() } else { book.name.clone() };
                    let hint = match (already, book.writable) {
                        (true, _) => "Already added",
                        (false, false) => words.read_only,
                        (false, true) => "",
                    };
                    ui.add_enabled_ui(!already, |ui| {
                        // Already synced: shown on, and it cannot be turned off here.
                        let mut shown = *on || already;
                        views::switch_row(ui, t, &mut shown, &label, hint);
                        *on = shown;
                    });
                }
            }
            Some(Err(why)) => {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(why.as_str()).color(t.danger)).wrap());
            }
            None => {}
        }
        ui.add_space(12.0);
        let ready = !form.server.trim().is_empty() && !form.user.trim().is_empty() && !form.password.is_empty();
        let chosen: Option<Vec<Found>> = match &form.found {
            Some(Ok(found)) => Some(found.iter().filter(|(book, on)| *on && !added.contains(&book.url.as_str())).map(|(book, _)| book.clone()).collect()),
            _ => None,
        };
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            match &chosen {
                Some(chosen) => {
                    let label = match chosen.len() {
                        0 | 1 => format!("Add {}", words.one),
                        n => format!("Add {n} {}", words.many),
                    };
                    let can = !chosen.is_empty();
                    if views::primary_button(ui, t, &label, can).on_disabled_hover_text(format!("Turn on the {} to sync", words.many)).clicked() || (can && enter) {
                        action = Some(BookAction::Add(chosen.clone()));
                    }
                }
                None => {
                    let label = if form.finding { "Finding…".to_owned() } else { format!("Find {}", words.many) };
                    let can = ready && !form.finding;
                    if views::primary_button(ui, t, &label, can).on_disabled_hover_text(if form.finding { "Asking the server" } else { "Server, user and password are required" }).clicked() || (can && enter) {
                        action = Some(BookAction::Find);
                    }
                    if form.finding {
                        ui.spinner();
                    }
                }
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if close || modal.should_close() {
        *state = None;
    }
    action
}

/// "Address books" from "address books".
fn capital(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_and_the_add_dialog_lay_out_and_skip_what_is_added() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let added = [AddressBook { name: "Personal".into(), url: "https://dav.example.org/ada/personal/".into(), user: "ada".into(), default: true, read_only: false }];
        let book = |url: &str, name: &str, writable| (Found { url: url.into(), name: name.into(), writable, color: String::new() }, writable);
        let found = vec![book(&added[0].url, "Personal", true), book("https://dav.example.org/ada/recent/", "Recently contacted", false)];
        let mut state = Some(AddBook { server: "example.org".into(), user: "ada".into(), password: "pw".into(), finding: false, focus_first: true, found: Some(Ok(found)) });
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 900.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                assert!(section(ui, &Tokens::dark(), &added, &Default::default(), None, &Default::default(), 0, &Default::default()).is_none(), "nothing pressed");
                assert!(add_dialog(ui.ctx(), &Tokens::dark(), &mut state, &[added[0].url.as_str()], &BOOKS).is_none());
            });
            output.textures_delta.clear();
        }
        let shown = ctx.memory(|m| m.area_rect(egui::Id::new("add_book"))).expect("the dialog is shown");
        assert!(shown.width() < 600.0 && shown.height() < 700.0, "{shown:?}");
        assert_eq!(host("https://dav.example.org/ada/"), "dav.example.org");
        let google = AddressBook { name: "Personal".into(), url: "https://www.googleapis.com/carddav/v1/principals/ada/lists/default/".into(), user: "ada".into(), default: false, read_only: false };
        let both = [added[0].clone(), google];
        assert_eq!((label(&both[0], &both), label(&both[0], &added)), ("Personal (dav.example.org)".to_owned(), "Personal".to_owned()), "the server only to tell two of one name apart");
        let bob = AddressBook { user: "bob".into(), url: "https://dav.example.org/bob/personal/".into(), ..added[0].clone() };
        assert_eq!(server(&added[0], &both), "dav.example.org");
        assert_eq!(server(&added[0], &[added[0].clone(), bob]), "ada@dav.example.org", "the login, when the server is there twice");
    }
}
