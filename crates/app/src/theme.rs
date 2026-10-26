//! Design tokens (light and dark) and the egui style. Widgets never hard-code colours.

use std::sync::Arc;

use beavermail_core::Role;
use eframe::egui::{self, Color32, CornerRadius, Margin, Shadow, Stroke, TextStyle, Theme, Visuals};

/// One geometry contract for every view: views never invent sizes.
pub const CONTROL_H: f32 = 40.0;
pub const CONTROL_RADIUS: u8 = 8;
/// Section titles inside a page: between body (15) and heading (22).
pub const SUBTITLE: f32 = 17.0;

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[derive(Clone, Copy)]
pub struct Tokens {
    pub dark: bool,
    /// App background (side panels).
    pub bg: Color32,
    /// Raised surfaces: windows, menus, cards.
    pub surface: Color32,
    pub input: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
}

impl Tokens {
    pub fn light() -> Self {
        Self {
            dark: false,
            bg: rgb(0xF6F6F8),
            surface: rgb(0xFFFFFF),
            input: rgb(0xFFFFFF),
            border: rgb(0xE4E4E9),
            border_strong: rgb(0xC9C9D1),
            text: rgb(0x18181B),
            text_dim: rgb(0x62626C),
            hover: rgb(0xEDEDF1),
            pressed: rgb(0xE1E1E7),
            accent: rgb(0x1763D6),
            on_accent: rgb(0xFFFFFF),
            danger: rgb(0xDC2626),
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            bg: rgb(0x141417),
            surface: rgb(0x1E1E23),
            input: rgb(0x121215),
            border: rgb(0x2E2E35),
            border_strong: rgb(0x474751),
            text: rgb(0xEDEDF0),
            text_dim: rgb(0x9C9CA8),
            hover: rgb(0x2A2A31),
            pressed: rgb(0x34343C),
            accent: rgb(0x2F7DF6),
            on_accent: rgb(0xFFFFFF),
            danger: rgb(0xF87171),
        }
    }

    pub fn get(ctx: &egui::Context) -> Self {
        if ctx.global_style().visuals.dark_mode { Self::dark() } else { Self::light() }
    }
}

fn visuals(t: &Tokens, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let radius = CornerRadius::same(CONTROL_RADIUS);
    v.panel_fill = t.bg;
    v.window_fill = t.surface;
    v.extreme_bg_color = t.input;
    v.faint_bg_color = t.hover;
    v.window_stroke = Stroke::new(1.0, t.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(if dark { 150 } else { 45 }) };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(if dark { 130 } else { 40 }) };
    v.selection.bg_fill = t.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, t.accent);
    v.hyperlink_color = t.accent;
    v.error_fg_color = t.danger;
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.text);
    // Inputs and buttons share `inactive`: a visible border is what makes a field read as a field.
    w.inactive.bg_fill = t.surface;
    w.inactive.weak_bg_fill = t.surface;
    w.inactive.bg_stroke = Stroke::new(1.0, t.border_strong);
    w.inactive.fg_stroke = Stroke::new(1.0, t.text);
    w.hovered.bg_fill = t.hover;
    w.hovered.weak_bg_fill = t.hover;
    w.hovered.bg_stroke = Stroke::new(1.0, t.text_dim);
    w.hovered.fg_stroke = Stroke::new(1.0, t.text);
    w.active.bg_fill = t.pressed;
    w.active.weak_bg_fill = t.pressed;
    w.active.bg_stroke = Stroke::new(1.0, t.accent);
    w.active.fg_stroke = Stroke::new(1.0, t.text);
    w.open.bg_fill = t.pressed;
    w.open.weak_bg_fill = t.pressed;
    for s in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        s.corner_radius = radius;
        s.expansion = 0.0;
    }
    v
}

/// The family for bold text. egui draws a variable font at its default weight only, and its
/// `strong()` changes the colour, not the weight: bold needs a font of its own.
pub fn bold() -> egui::FontFamily {
    egui::FontFamily::Name("bold".into())
}

/// Inter (SIL OFL, see `assets/fonts/Inter-LICENSE.txt`) first in the proportional family; egui's defaults stay as fallback for symbols.
/// `Inter-SemiBold.ttf` is the same font at weight 600 (`fonttools varLib.instancer InterVariable.ttf wght=600 opsz=14`).
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("Inter".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/InterVariable.ttf"))));
    fonts.font_data.insert("InterSemiBold".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf"))));
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "Inter".into());
    }
    let fallback = fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.insert(bold(), std::iter::once("InterSemiBold".to_owned()).chain(fallback).collect());
    ctx.set_fonts(fonts);
}

/// Install both themes; egui picks the one matching the system setting.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    for (theme, dark, tokens) in [(Theme::Light, false, Tokens::light()), (Theme::Dark, true, Tokens::dark())] {
        ctx.set_visuals_of(theme, visuals(&tokens, dark));
    }
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(16.0, 8.0);
        s.spacing.interact_size = egui::vec2(CONTROL_H, CONTROL_H);
        s.spacing.menu_margin = Margin::same(8);
        s.spacing.window_margin = Margin::same(20);
        // A list or page longer than its pane shows its scroll bar at rest, not only under the pointer.
        s.spacing.scroll.dormant_handle_opacity = 0.6;
        s.text_styles.insert(TextStyle::Body, egui::FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Button, egui::FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Small, egui::FontId::proportional(13.0));
        s.text_styles.insert(TextStyle::Heading, egui::FontId::new(22.0, bold()));
        s.interaction.tooltip_delay = 0.4;
        // What is inside a folded group (an account's folders, a server's calendars) is told by its indent alone.
        s.visuals.indent_has_left_vline = false;
    });
}

/// The sidebars' rows at `density`: their height and the gap between them. Set on the `ui` holding
/// them, for the rows and the account and server headers alike.
pub fn dense(ui: &mut egui::Ui, density: beavermail_core::Density) {
    use beavermail_core::Density;
    let (height, gap) = match density {
        Density::Compact => (28.0, 0.0),
        Density::Normal => (32.0, 2.0),
        Density::Comfortable => (38.0, 8.0),
    };
    ui.spacing_mut().interact_size.y = height;
    ui.spacing_mut().item_spacing.y = gap;
}

/// Folder icon colours: the same in both themes so a folder is recognisable at a glance.
pub fn role_color(role: Role) -> Color32 {
    match role {
        Role::Inbox => rgb(0x2F9CF4),
        Role::Starred => rgb(0xF5B301),
        Role::Drafts => rgb(0xD946EF),
        Role::Sent => rgb(0x00A8F3),
        Role::Archive => rgb(0x8B5CF6),
        Role::Junk => rgb(0xF59E0B),
        Role::Trash => rgb(0xFF5B4D),
        Role::Other => rgb(0x22A06B),
    }
}

/// A label's name and colour: the five standard keywords `$label1` to `$label5` with their usual names
/// and colours; any other keyword is a label of the user's own, named as written (`_` for a space), in
/// a neutral grey.
pub fn label(keyword: &str) -> (String, Color32) {
    match keyword {
        "$label1" => ("Important".into(), rgb(0xFF3B30)),
        "$label2" => ("Work".into(), rgb(0xFF9900)),
        "$label3" => ("Personal".into(), rgb(0x2EA043)),
        "$label4" => ("To Do".into(), rgb(0x4B5CFF)),
        "$label5" => ("Later".into(), rgb(0xA855C7)),
        other => (other.replace('_', " "), rgb(0x8E8E99)),
    }
}

/// A calendar's colour as the server keeps it (`#RRGGBB`); `None` when it has none or it cannot be read.
pub fn calendar_color(hex: &str) -> Option<Color32> {
    let hex = hex.trim().strip_prefix('#')?;
    let value = u32::from_str_radix(hex.get(..6)?, 16).ok()?;
    Some(rgb(value))
}

/// The colour marking an account's mail in All inboxes and beside its name, by its place in the list.
pub fn account_color(at: usize) -> Color32 {
    const HUES: [u32; 6] = [0x3B82F6, 0xF59E0B, 0x22A06B, 0xEC4899, 0x8B5CF6, 0x06B6D4];
    rgb(HUES[at % HUES.len()])
}

/// Background and initials colour of a person's avatar, stable for a given name.
pub fn avatar_colors(name: &str, dark: bool) -> (Color32, Color32) {
    const HUES: [u32; 6] = [0x3B82F6, 0x8B5CF6, 0xEC4899, 0xF59E0B, 0x22A06B, 0x06B6D4];
    let hash = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    let hue = rgb(HUES[hash as usize % HUES.len()]);
    if dark { (hue.gamma_multiply(0.28), hue) } else { (hue.gamma_multiply(0.18), hue.gamma_multiply(0.85)) }
}
