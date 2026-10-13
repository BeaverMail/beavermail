//! Lucide icons (see `assets/icons/README.md`), drawn as tinted SVG images.

use eframe::egui::{self, Color32, CursorIcon, Image, Response, Sense, Ui, Vec2, vec2};

use crate::theme::{CONTROL_RADIUS, Tokens};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Archive,
    Drafts,
    Folder,
    Inbox,
    Mail,
    MailOpen,
    Pencil,
    Plus,
    Refresh,
    Search,
    Send,
    Spam,
    Star,
    Trash,
    X,
}

impl Icon {
    fn source(self) -> egui::ImageSource<'static> {
        match self {
            Self::Archive => egui::include_image!("../assets/icons/archive.svg"),
            Self::Drafts => egui::include_image!("../assets/icons/file-pen-line.svg"),
            Self::Folder => egui::include_image!("../assets/icons/folder.svg"),
            Self::Inbox => egui::include_image!("../assets/icons/inbox.svg"),
            Self::Mail => egui::include_image!("../assets/icons/mail.svg"),
            Self::MailOpen => egui::include_image!("../assets/icons/mail-open.svg"),
            Self::Pencil => egui::include_image!("../assets/icons/pencil.svg"),
            Self::Plus => egui::include_image!("../assets/icons/plus.svg"),
            Self::Refresh => egui::include_image!("../assets/icons/refresh-cw.svg"),
            Self::Search => egui::include_image!("../assets/icons/search.svg"),
            Self::Send => egui::include_image!("../assets/icons/send.svg"),
            Self::Spam => egui::include_image!("../assets/icons/triangle-alert.svg"),
            Self::Star => egui::include_image!("../assets/icons/star.svg"),
            Self::Trash => egui::include_image!("../assets/icons/trash-2.svg"),
            Self::X => egui::include_image!("../assets/icons/x.svg"),
        }
    }

    pub fn image(self, size: f32, color: Color32) -> Image<'static> {
        Image::new(self.source()).fit_to_exact_size(Vec2::splat(size)).tint(color)
    }
}

/// Standard inline icon size.
pub const SIZE: f32 = 18.0;

/// A square, borderless icon button; `active` draws it pressed in (a toggled state).
pub fn button(ui: &mut Ui, t: &Tokens, icon: Icon, tip: &str, enabled: bool, active: bool) -> Response {
    let side = crate::theme::CONTROL_H - 4.0;
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::click());
    let response = if enabled { response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip) } else { response.on_disabled_hover_text(tip) };
    let fill = if !enabled {
        None
    } else if response.is_pointer_button_down_on() || active {
        Some(t.pressed)
    } else if response.hovered() {
        Some(t.hover)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(CONTROL_RADIUS), fill);
    }
    let color = if enabled { t.text_dim } else { t.text_dim.gamma_multiply(0.4) };
    icon.image(SIZE, color).paint_at(ui, egui::Rect::from_center_size(rect.center(), Vec2::splat(SIZE)));
    if enabled { response } else { response.interact(Sense::hover()) }
}

/// A small icon button for inside fields: no hover plate bigger than the icon.
pub fn button_small(ui: &mut Ui, t: &Tokens, icon: Icon, tip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    let color = if response.hovered() { t.text } else { t.text_dim };
    icon.image(14.0, color).paint_at(ui, egui::Rect::from_center_size(rect.center(), Vec2::splat(14.0)));
    response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip)
}
