//! Lucide icons (see `assets/icons/README.md`), drawn as tinted SVG images.

use eframe::egui::{self, Color32, CursorIcon, Image, Response, Sense, Ui, Vec2, vec2};

use crate::theme::{CONTROL_RADIUS, Tokens};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Archive,
    Ban,
    ArrowDown,
    Calendar,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    CircleAlert,
    Drafts,
    Ellipsis,
    FileCode,
    Folder,
    Forward,
    Inbox,
    Key,
    Lock,
    Mail,
    MailOpen,
    MapPin,
    Paperclip,
    Palette,
    Pencil,
    Pin,
    Plus,
    Refresh,
    Reply,
    ReplyAll,
    Search,
    Send,
    Settings,
    Shield,
    ShieldCheck,
    Spam,
    Star,
    Trash,
    Users,
    X,
}

impl Icon {
    fn source(self) -> egui::ImageSource<'static> {
        match self {
            Self::Archive => egui::include_image!("../assets/icons/archive.svg"),
            Self::Ban => egui::include_image!("../assets/icons/ban.svg"),
            Self::ArrowDown => egui::include_image!("../assets/icons/arrow-down.svg"),
            Self::Calendar => egui::include_image!("../assets/icons/calendar-days.svg"),
            Self::ChevronDown => egui::include_image!("../assets/icons/chevron-down.svg"),
            Self::ChevronLeft => egui::include_image!("../assets/icons/chevron-left.svg"),
            Self::ChevronRight => egui::include_image!("../assets/icons/chevron-right.svg"),
            Self::ChevronUp => egui::include_image!("../assets/icons/chevron-up.svg"),
            Self::CircleAlert => egui::include_image!("../assets/icons/circle-alert.svg"),
            Self::Drafts => egui::include_image!("../assets/icons/file-pen-line.svg"),
            Self::Ellipsis => egui::include_image!("../assets/icons/ellipsis.svg"),
            Self::FileCode => egui::include_image!("../assets/icons/file-code.svg"),
            Self::Folder => egui::include_image!("../assets/icons/folder.svg"),
            Self::Forward => egui::include_image!("../assets/icons/forward.svg"),
            Self::Inbox => egui::include_image!("../assets/icons/inbox.svg"),
            Self::Key => egui::include_image!("../assets/icons/key-round.svg"),
            Self::Lock => egui::include_image!("../assets/icons/lock-keyhole.svg"),
            Self::Mail => egui::include_image!("../assets/icons/mail.svg"),
            Self::MapPin => egui::include_image!("../assets/icons/map-pin.svg"),
            Self::MailOpen => egui::include_image!("../assets/icons/mail-open.svg"),
            Self::Paperclip => egui::include_image!("../assets/icons/paperclip.svg"),
            Self::Palette => egui::include_image!("../assets/icons/palette.svg"),
            Self::Pencil => egui::include_image!("../assets/icons/pencil.svg"),
            Self::Pin => egui::include_image!("../assets/icons/pin.svg"),
            Self::Plus => egui::include_image!("../assets/icons/plus.svg"),
            Self::Refresh => egui::include_image!("../assets/icons/refresh-cw.svg"),
            Self::Reply => egui::include_image!("../assets/icons/reply.svg"),
            Self::ReplyAll => egui::include_image!("../assets/icons/reply-all.svg"),
            Self::Search => egui::include_image!("../assets/icons/search.svg"),
            Self::Send => egui::include_image!("../assets/icons/send.svg"),
            Self::Settings => egui::include_image!("../assets/icons/settings.svg"),
            Self::Shield => egui::include_image!("../assets/icons/shield.svg"),
            Self::ShieldCheck => egui::include_image!("../assets/icons/shield-check.svg"),
            Self::Spam => egui::include_image!("../assets/icons/triangle-alert.svg"),
            Self::Star => egui::include_image!("../assets/icons/star.svg"),
            Self::Trash => egui::include_image!("../assets/icons/trash-2.svg"),
            Self::Users => egui::include_image!("../assets/icons/users.svg"),
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

/// A small icon button for inside fields: no hover plate bigger than the icon. Tab passes it by, so
/// it does not sit between a field and the next (removing a chip or a file has its key: Backspace).
pub fn button_small(ui: &mut Ui, t: &Tokens, icon: Icon, tip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::CLICK);
    let color = if response.hovered() { t.text } else { t.text_dim };
    icon.image(14.0, color).paint_at(ui, egui::Rect::from_center_size(rect.center(), Vec2::splat(14.0)));
    response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip)
}
