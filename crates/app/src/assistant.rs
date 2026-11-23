// SPDX-License-Identifier: AGPL-3.0-only

//! The assistant's settings page, the question asked before mail leaves for a language model, and
//! the record of what was sent, kept in the vault like the mail itself.

use std::path::Path;

use beavermail_core::{Assistant, LlmProvider, MessageId};
use beavermail_vault::Vault;
use eframe::egui::{self, Align, Layout, RichText, Ui, vec2};
use serde::{Deserialize, Serialize};

use crate::llm;
use crate::theme::{self, Tokens};
use crate::views;

const LOG: &str = "assistant";

/// What a request is for, so its answer reaches the right place.
#[derive(Clone, Debug)]
pub enum Purpose {
    /// The message being written, by its draft id.
    Reply(String),
    /// The conversation of this message.
    Summary(MessageId),
}

impl Purpose {
    fn what(&self) -> &'static str {
        match self {
            Self::Reply(_) => "Write a message",
            Self::Summary(_) => "Summarise",
        }
    }
}

/// A request ready to go: what it is for, and every word that would leave this computer.
pub struct Prepared {
    pub purpose: Purpose,
    pub system: &'static str,
    pub input: String,
    /// Messages of the conversation in it.
    pub messages: usize,
}

/// The page's state between frames.
#[derive(Default)]
pub struct State {
    /// The key and address as typed, and the provider they were read for.
    key: String,
    url: String,
    key_for: Option<LlmProvider>,
    key_error: Option<String>,
    pub models: Option<Result<Vec<(String, String)>, String>>,
    pub loading: bool,
    pub log: Option<Result<Vec<Entry>, String>>,
    /// Waiting for the user's yes.
    pub confirm: Option<Prepared>,
    show_sent: bool,
    /// "Don't ask again" is ticked in the question.
    pub trust: bool,
}

/// What the page asks the app to do.
pub enum PageAction {
    LoadModels,
    ShowLog,
    ClearLog,
}

/// One request as it left: when, where, and the text sent.
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub at: i64,
    pub provider: String,
    pub model: String,
    pub what: String,
    pub sent: String,
}

/// Ready to use: a provider and a model picked.
pub fn ready(settings: &Assistant) -> bool {
    settings.provider.is_some() && !settings.model.is_empty()
}

/// Where requests go now, with the key from the credential store. Blocking (the store may ask the system).
pub fn target(settings: &Assistant) -> Result<llm::Target, String> {
    let provider = settings.provider.ok_or("the assistant is off")?;
    if settings.model.is_empty() {
        return Err("choose a model in Settings › Assistant".into());
    }
    let key = beavermail_accounts::assistant_key(provider)?;
    if key.is_empty() && provider != LlmProvider::Local {
        return Err("add the API key in Settings › Assistant".into());
    }
    Ok(llm::Target { provider, url: settings.url.clone(), key, model: settings.model.clone() })
}

/// Sends `prepared` and keeps a record of it first: what left is recorded even when no answer comes.
pub fn send(vault: &Vault, settings: &Assistant, prepared: &Prepared) -> Result<String, String> {
    let target = target(settings)?;
    let entry = Entry {
        at: chrono::Utc::now().timestamp(),
        provider: target.provider.name().into(),
        model: target.model.clone(),
        what: prepared.purpose.what().into(),
        sent: format!("{}\n\n{}", prepared.system, prepared.input),
    };
    let path = Path::new(LOG).join(format!("{}.json", chrono::Utc::now().timestamp_millis()));
    let json = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
    vault.write(&path, &json).map_err(|e| format!("could not keep the record of what is sent: {e}"))?;
    let began = std::time::Instant::now();
    let result = llm::complete(&target, prepared.system, &prepared.input);
    log::debug!("assistant: {} answered in {:.0?}{}", target.provider.key(), began.elapsed(), if result.is_err() { " with an error" } else { "" });
    result
}

/// The record, newest first.
pub fn read_log(vault: &Vault) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for path in vault.list(Path::new(LOG)).map_err(|e| e.to_string())? {
        if let Some(entry) = vault.read(&path).map_err(|e| e.to_string())?.and_then(|bytes| serde_json::from_slice::<Entry>(&bytes).ok()) {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.at));
    Ok(entries)
}

pub fn clear_log(vault: &Vault) -> Result<(), String> {
    vault.remove_dir(Path::new(LOG)).map_err(|e| e.to_string())
}

fn hint(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.label(RichText::new(text).size(13.0).color(t.text_dim));
}

/// Settings › Assistant. True when the settings changed.
pub fn page(ui: &mut Ui, t: &Tokens, settings: &mut Assistant, state: &mut State) -> (bool, Option<PageAction>) {
    let (mut changed, mut action) = (false, None);
    let before = settings.provider;
    crate::form_grid(ui, "assistant_form", |ui| {
        ui.label("Provider");
        egui::ComboBox::from_id_salt("assistant_provider").width(crate::FIELD_WIDTH).selected_text(settings.provider.map_or("Off", LlmProvider::name)).show_ui(ui, |ui| {
            ui.selectable_value(&mut settings.provider, None, "Off");
            for provider in LlmProvider::ALL {
                ui.selectable_value(&mut settings.provider, Some(provider), provider.name());
            }
        });
        ui.end_row();
    });
    if settings.provider != before {
        // Another provider has its own address and models, and is asked about again.
        (settings.url, settings.model, settings.trusted, state.models) = (String::new(), String::new(), false, None);
        changed = true;
    }
    let Some(provider) = settings.provider else { return (changed, None) };
    // What is typed, read again for another provider; kept only with Save.
    if state.key_for != Some(provider) {
        (state.key_for, state.key_error, state.url) = (Some(provider), None, settings.url.clone());
        state.key = beavermail_accounts::assistant_key(provider).unwrap_or_else(|e| {
            state.key_error = Some(e);
            String::new()
        });
    }
    let local = provider == LlmProvider::Local;
    ui.add_space(12.0);
    crate::form_grid(ui, "assistant_server", |ui| {
        ui.label("Address");
        ui.add(crate::field(&mut state.url).hint_text(llm::default_url(provider)));
        ui.end_row();
        ui.label("API key");
        ui.add(crate::field(&mut state.key).password(true).hint_text(if local { "Only if the server asks for one" } else { "" }));
        ui.end_row();
    });
    let about = if local {
        "Any server speaking OpenAI's API: Ollama, LM Studio, llama.cpp. Empty: Ollama on this computer. The key is kept in the system's credential store."
    } else {
        "Empty: the provider's own address; another one for a gateway speaking the same API. The key is kept in the system's credential store."
    };
    hint(ui, t, about);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let button = views::primary_button(ui, t, "Save and load models", !state.loading);
        if button.clicked() {
            let url = state.url.trim().trim_end_matches('/').to_owned();
            if url != settings.url {
                (settings.url, settings.model) = (url.clone(), String::new());
                changed = true;
            }
            state.url = url;
            state.key = state.key.trim().to_owned();
            state.key_error = beavermail_accounts::set_assistant_key(provider, &state.key).err();
            if state.key_error.is_none() {
                action = Some(PageAction::LoadModels);
            }
        }
        if state.loading {
            ui.spinner();
        }
    });
    if let Some(e) = &state.key_error {
        ui.label(RichText::new(e).size(13.0).color(t.danger));
    }
    if let Some(Err(e)) = &state.models {
        ui.add(egui::Label::new(RichText::new(format!("Could not load the models: {e}")).size(13.0).color(t.danger)).wrap());
    }
    ui.add_space(12.0);
    crate::form_grid(ui, "assistant_model", |ui| {
        ui.label("Model");
        match &state.models {
            Some(Ok(models)) if !models.is_empty() => {
                let shown = models.iter().find(|(id, _)| *id == settings.model).map_or(if settings.model.is_empty() { "Choose a model" } else { settings.model.as_str() }, |(_, name)| name.as_str());
                egui::ComboBox::from_id_salt("assistant_model").width(crate::FIELD_WIDTH).selected_text(shown).show_ui(ui, |ui| {
                    for (id, name) in models {
                        changed |= ui.selectable_value(&mut settings.model, id.clone(), name).changed();
                    }
                });
            }
            Some(Ok(_)) => {
                ui.label(RichText::new("The server offers no models").color(t.text_dim));
            }
            _ if !settings.model.is_empty() => {
                ui.label(&settings.model);
            }
            _ => {
                ui.label(RichText::new("Save to load the models the key can use").color(t.text_dim));
            }
        }
        ui.end_row();
    });
    ui.add_space(12.0);
    let mut ask = !settings.trusted;
    let about = "Before mail leaves, shows where it goes and how much of it. Off: sent as soon as you ask.";
    if views::switch_row(ui, t, &mut ask, "Ask before sending mail to the assistant", about).changed() {
        settings.trusted = !ask;
        changed = true;
    }
    ui.add_space(12.0);
    crate::section_title(ui, "What was sent");
    match &state.log {
        None => {
            if ui.button("Show what was sent").clicked() {
                action = Some(PageAction::ShowLog);
            }
        }
        Some(Err(e)) => {
            ui.label(RichText::new(e).size(13.0).color(t.danger));
        }
        Some(Ok(entries)) if entries.is_empty() => hint(ui, t, "Nothing sent yet."),
        Some(Ok(entries)) => {
            for (n, entry) in entries.iter().enumerate() {
                let when = chrono::DateTime::from_timestamp(entry.at, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                egui::CollapsingHeader::new(format!("{when}  ·  {}  ·  {} {}", entry.what, entry.provider, entry.model)).id_salt(("assistant_log", n)).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(240.0).id_salt(("assistant_sent", n)).show(ui, |ui| {
                        ui.label(RichText::new(&entry.sent).size(12.5).color(t.text_dim));
                    });
                });
            }
            ui.add_space(8.0);
            if ui.button("Delete the record").clicked() {
                action = Some(PageAction::ClearLog);
            }
        }
    }
    (changed, action)
}

/// The question before mail leaves. `Some(true)` to send, `Some(false)` to cancel, `None` while open;
/// [`State::trust`] is set when the user asks not to be asked again.
pub fn confirm(ctx: &egui::Context, t: &Tokens, settings: &Assistant, state: &mut State) -> Option<bool> {
    let prepared = state.confirm.as_ref()?;
    let provider = settings.provider.map_or("", LlmProvider::name);
    let (mut send, mut cancel) = (false, false);
    let modal = views::modal(ctx, egui::Id::new("confirm_assistant")).show(ctx, |ui| {
        ui.set_width(crate::FORM_WIDTH);
        ui.heading("Send to the assistant?");
        let messages = match prepared.messages {
            0 => "your text only".to_owned(),
            1 => "1 message".to_owned(),
            n => format!("{n} messages"),
        };
        let chars = prepared.input.chars().count();
        ui.label(RichText::new(format!("{}: {messages}, {chars} characters, go to {provider} ({}).", prepared.purpose.what(), settings.model)).color(t.text_dim));
        ui.add_space(8.0);
        egui::CollapsingHeader::new("What is sent").open(Some(state.show_sent)).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                ui.label(RichText::new(&prepared.input).size(12.5).color(t.text_dim));
            });
        });
        if ui.small_button(if state.show_sent { "Hide" } else { "Show what is sent" }).clicked() {
            state.show_sent = !state.show_sent;
        }
        ui.add_space(8.0);
        ui.checkbox(&mut state.trust, format!("Don't ask again for {provider}"));
        ui.add_space(12.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), theme::CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
            send = views::primary_button(ui, t, "Send", true).clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if send {
        return Some(true);
    }
    (cancel || modal.should_close()).then_some(false)
}

/// The Summarise button; true when pressed.
pub fn summary_button(ui: &mut Ui, t: &Tokens, tip: &str) -> bool {
    let button = egui::Button::image_and_text(crate::icons::Icon::Sparkles.image(crate::icons::SIZE, t.text_dim), "Summarise");
    ui.add(button).on_hover_text(tip).clicked()
}

/// The summary above the messages: being written (`None`), written, or why not. True when closed.
pub fn summary_box(ui: &mut Ui, t: &Tokens, state: &Option<Result<String, String>>) -> bool {
    let mut close = false;
    egui::Frame::new().fill(t.surface).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(8).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            views::note(ui, crate::icons::Icon::Sparkles, t.text_dim, "Summary by the assistant", "Written by a language model: it can be wrong");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close = crate::icons::button_small(ui, t, crate::icons::Icon::X, "Close the summary").clicked();
            });
        });
        match state {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Summarising…").color(t.text_dim));
                });
            }
            Some(Ok(text)) => {
                crate::body::body(ui, t, &crate::html::blocks(&beavermail_smtp::markdown_html(text)));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("Could not summarise: {e}")).size(13.0).color(t.danger));
            }
        }
    });
    ui.add_space(8.0);
    close
}
