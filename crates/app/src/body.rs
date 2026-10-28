//! A message body as one text the reader selects from end to end: dragged either way, from the room
//! beside a line too, extended with Shift+click after scrolling, a word with a double click.
//!
//! egui's labels drop a selection as soon as its start scrolls out of sight, and one starts only on
//! a label's own text. Here every block is laid out each frame and the selection is kept as places in
//! the text, whatever is on screen.

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Galley, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};

use crate::html::{Block, Kind, Run};
use crate::theme::{self, CONTROL_H, CONTROL_RADIUS, Tokens};
use crate::views::{self, LinkInfo};

/// Room under each line of text, as between the labels this replaces.
const ROW_GAP: f32 = 2.0;
/// Room between the columns of a table.
const TABLE_GAP: f32 = 16.0;
const TABLE_MIN_W: f32 = 48.0;

/// A message body as blocks. Returns the link the user clicked, already cleaned of tracking parameters.
pub fn body(ui: &mut Ui, t: &Tokens, blocks: &[Block]) -> Option<String> {
    show(ui, t, blocks, None).0
}

/// A message body with the earlier messages it carries (from block `fold` on, see
/// [`crate::html::quoted_tail`]) folded behind "Show quoted text", which `shown` remembers. Returns the link clicked.
pub fn reply_body(ui: &mut Ui, t: &Tokens, blocks: &[Block], fold: Option<usize>, shown: &mut bool) -> Option<String> {
    let Some(at) = fold.filter(|&at| at < blocks.len()) else {
        return body(ui, t, blocks);
    };
    // The button sits in room left inside the text, so a selection runs across it.
    let blocks = if *shown { blocks } else { &blocks[..at] };
    let room = 4.0 + CONTROL_H + if *shown { 12.0 } else { 0.0 };
    let (clicked, gap) = show(ui, t, blocks, Some((at, room)));
    if let Some(gap) = gap {
        let (label, tip) = if *shown { ("Hide quoted text", "Fold the earlier messages away again") } else { ("Show quoted text", "The earlier messages this one carries below it") };
        let place = Rect::from_min_size(gap.min + vec2(0.0, 4.0), vec2(gap.width(), CONTROL_H));
        if ui.scope_builder(egui::UiBuilder::new().max_rect(place), |ui| ui.button(label).on_hover_text(tip)).inner.clicked() {
            *shown = !*shown;
        }
    }
    clicked
}

/// A place in the body's text: before character `index` of piece `piece`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct At {
    piece: usize,
    index: usize,
}

#[derive(Clone, Default)]
struct State {
    /// Where the selection started; `head` is where it ends, moved by dragging or Shift+click.
    anchor: Option<At>,
    head: At,
    dragging: bool,
    /// The link under the pointer at the last frame, as (piece, link): underlined.
    hovered: Option<(usize, usize)>,
    /// The link last right-clicked, cleaned, for its menu.
    menu_link: Option<String>,
}

impl State {
    /// The selection, from its first place to its last; `None` when nothing is selected.
    fn range(&self) -> Option<(At, At)> {
        let anchor = self.anchor?;
        (anchor != self.head).then(|| (anchor.min(self.head), anchor.max(self.head)))
    }
}

/// Text laid out in one place: a paragraph, a list item's text, a table cell.
struct Piece {
    galley: Arc<Galley>,
    /// Its top left, from the body's.
    at: Pos2,
    /// Its links: where they are in its text, with their text and target.
    links: Vec<(Range<usize>, String, String)>,
    /// Copied before its text when the selection takes its start: a list item's marker.
    prefix: String,
    /// Copied after it when the selection goes on: a line break, a blank line, a tab between cells.
    after: &'static str,
}

impl Piece {
    fn chars(&self) -> usize {
        self.galley.end().index.0
    }

    fn rect(&self) -> Rect {
        Rect::from_min_size(self.at, self.galley.size())
    }
}

/// What is drawn around the text, never selected.
enum Mark {
    /// A list item's marker.
    Text(Pos2, Arc<Galley>),
    Line(Pos2, Pos2, Stroke),
    Box(Rect, Color32),
}

/// How a block's text looks.
#[derive(Clone, Copy)]
struct Look {
    size: f32,
    color: Color32,
    mono: bool,
    strong: bool,
}

impl Look {
    fn new(size: f32, color: Color32) -> Self {
        Self { size, color, mono: false, strong: false }
    }
}

/// The blocks laid out for one width, from the body's top left.
struct Laid {
    pieces: Vec<Piece>,
    marks: Vec<Mark>,
    height: f32,
    /// The room left for a control (see [`show`]).
    gap: Option<Rect>,
}

impl Laid {
    fn new(ui: &Ui, t: &Tokens, blocks: &[Block], width: f32, gap: Option<(usize, f32)>, hovered: Option<(usize, usize)>) -> Self {
        let mut laid = Self { pieces: Vec::new(), marks: Vec::new(), height: 0.0, gap: None };
        let mut y = 0.0;
        for (i, block) in blocks.iter().enumerate() {
            if let Some((at, room)) = gap
                && at == i
            {
                laid.gap = Some(Rect::from_min_size(pos2(0.0, y), vec2(width, room)));
                y += room;
            }
            let indent = f32::from(block.depth) * 14.0;
            match block.kind {
                Kind::Rule => {
                    laid.marks.push(Mark::Line(pos2(0.0, y + 3.0), pos2(width, y + 3.0), ui.visuals().widgets.noninteractive.bg_stroke));
                    y += 6.0 + ROW_GAP;
                }
                Kind::Heading(level) => {
                    y += 6.0;
                    let look = Look { strong: true, ..Look::new(heading_size(level), t.text) };
                    y += laid.put(ui, t, &block.runs, look, pos2(0.0, y), width, hovered, String::new(), "\n") + ROW_GAP + 4.0;
                }
                Kind::Item => {
                    let marker = block.marker.as_deref().unwrap_or("•");
                    let galley = ui.painter().layout_no_wrap(marker.to_owned(), FontId::proportional(16.0), t.text_dim);
                    // A continued line keeps the marker's room, without it, so it lines up with the item's text.
                    let x = indent + galley.size().x + ui.spacing().item_spacing.x;
                    let (prefix, marker_h) = if block.continued { (String::new(), 0.0) } else { (format!("{marker} "), galley.size().y) };
                    if !block.continued {
                        laid.marks.push(Mark::Text(pos2(indent, y), galley));
                    }
                    let h = laid.put(ui, t, &block.runs, Look::new(16.0, t.text), pos2(x, y), width - x, hovered, prefix, "\n");
                    y += h.max(marker_h) + ROW_GAP + 2.0;
                }
                Kind::Quote => {
                    let x = 12.0 + indent;
                    let h = laid.put(ui, t, &block.runs, Look::new(15.5, t.text_dim), pos2(x, y), width - x, hovered, String::new(), "\n");
                    laid.marks.push(Mark::Line(pos2(indent, y), pos2(indent, y + h), Stroke::new(2.0, t.border_strong)));
                    y += h + ROW_GAP;
                }
                Kind::Pre => {
                    // A code block in its own box, as wide as the text column.
                    let look = Look { mono: true, ..Look::new(14.5, t.text) };
                    let at = laid.marks.len();
                    let h = laid.put(ui, t, &block.runs, look, pos2(12.0, y + 8.0), width - 24.0, hovered, String::new(), "\n");
                    laid.marks.insert(at, Mark::Box(Rect::from_min_size(pos2(0.0, y), vec2(width, h + 16.0)), t.hover));
                    y += h + 16.0 + ROW_GAP + 6.0;
                }
                Kind::Table { header } => {
                    y = laid.table(ui, t, &block.rows, header, y, width, hovered) + 10.0;
                }
                Kind::Para | Kind::Line if block.runs.is_empty() => y += 8.0,
                Kind::Para | Kind::Line => {
                    let after = if block.kind == Kind::Para { "\n\n" } else { "\n" };
                    y += laid.put(ui, t, &block.runs, Look::new(16.0, t.text), pos2(0.0, y), width, hovered, String::new(), after) + ROW_GAP;
                    if block.kind == Kind::Para {
                        y += 8.0;
                    }
                }
            }
        }
        if let Some((at, room)) = gap
            && at >= blocks.len()
        {
            laid.gap = Some(Rect::from_min_size(pos2(0.0, y), vec2(width, room)));
            y += room;
        }
        laid.height = y;
        laid
    }

    /// Lays `runs` out at `at`, `width` wide, as the next piece; returns its height.
    #[allow(clippy::too_many_arguments)]
    fn put(&mut self, ui: &Ui, t: &Tokens, runs: &[Run], look: Look, at: Pos2, width: f32, hovered: Option<(usize, usize)>, prefix: String, after: &'static str) -> f32 {
        let hovered = hovered.filter(|(piece, _)| *piece == self.pieces.len()).map(|(_, link)| link);
        let mut job = LayoutJob::default();
        job.wrap.max_width = width.max(1.0);
        let mut links = Vec::new();
        let mut chars = 0;
        for run in runs {
            let (shown, format) = match run {
                Run::Text { text, bold, italic, code, strike } => {
                    let inline_code = *code && !look.mono;
                    let family = if look.mono || *code {
                        FontFamily::Monospace
                    } else if *bold || look.strong {
                        theme::bold()
                    } else {
                        FontFamily::Proportional
                    };
                    // Inline code stands apart from the words around it, not only by its font.
                    let size = if inline_code { look.size - 1.0 } else { look.size };
                    let mut format = TextFormat { font_id: FontId::new(size, family), color: look.color, italics: *italic, ..Default::default() };
                    if inline_code {
                        format.background = t.hover;
                    }
                    if *strike {
                        format.strikethrough = Stroke::new(1.0, look.color);
                    }
                    (text.as_str(), format)
                }
                Run::Link { text, url } => {
                    let tint = if LinkInfo::new(ui.ctx(), text, url).misleading { t.danger } else { t.accent };
                    let mut format = TextFormat { font_id: FontId::proportional(look.size), color: tint, ..Default::default() };
                    if hovered == Some(links.len()) {
                        format.underline = Stroke::new(1.0, tint);
                    }
                    let shown = if text.is_empty() { url } else { text };
                    let n = shown.chars().count();
                    links.push((chars..chars + n, text.clone(), url.clone()));
                    (shown.as_str(), format)
                }
            };
            chars += shown.chars().count();
            job.append(shown, 0.0, format);
        }
        let galley = ui.painter().layout_job(job);
        let height = galley.size().y;
        self.pieces.push(Piece { galley, at, links, prefix, after });
        height
    }

    /// A table of data: columns as wide as their longest cell asks, within the reader's width, a line
    /// under the header and between rows. Returns where it ends.
    #[allow(clippy::too_many_arguments)]
    fn table(&mut self, ui: &Ui, t: &Tokens, rows: &[Vec<Vec<Run>>], header: bool, mut y: f32, width: f32, hovered: Option<(usize, usize)>) -> f32 {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let length = |cell: &[Run]| cell.iter().map(|r| match r { Run::Text { text, .. } | Run::Link { text, .. } => text.chars().count() }).sum::<usize>();
        let wants: Vec<f32> = (0..columns).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|cell| length(cell)).max().unwrap_or(0).max(4) as f32).collect();
        let room = (width - TABLE_GAP * (columns.saturating_sub(1)) as f32).max(TABLE_MIN_W * columns as f32);
        let total: f32 = wants.iter().sum();
        let widths: Vec<f32> = wants.iter().map(|w| (room * w / total).max(TABLE_MIN_W)).collect();
        let full = widths.iter().sum::<f32>() + TABLE_GAP * (columns.saturating_sub(1)) as f32;
        for (i, row) in rows.iter().enumerate() {
            let color = if header && i == 0 { t.text_dim } else { t.text };
            let (mut x, mut row_h) = (0.0, 0.0_f32);
            for (c, (cell, w)) in row.iter().zip(&widths).enumerate() {
                let after = if c + 1 < row.len() { "\t" } else { "\n" };
                row_h = row_h.max(self.put(ui, t, cell, Look::new(15.0, color), pos2(x, y), *w, hovered, String::new(), after));
                x += w + TABLE_GAP;
            }
            y += row_h + ROW_GAP;
            if i + 1 < rows.len() {
                let stroke = if header && i == 0 { Stroke::new(1.0, t.border_strong) } else { Stroke::new(1.0, t.border) };
                let line_y = y - ROW_GAP + 3.0;
                self.marks.push(Mark::Line(pos2(0.0, line_y), pos2(full, line_y), stroke));
                y += 6.0;
            }
        }
        y
    }

    /// The place in the text nearest `p`: in the piece level with it (the nearest across, for a
    /// table's cells), else the start of the next piece down, else the very end.
    fn at(&self, p: Pos2) -> At {
        let across = |piece: &Piece| {
            let r = piece.rect();
            (r.left() - p.x).max(p.x - r.right()).max(0.0)
        };
        let level = self.pieces.iter().enumerate().filter(|(_, x)| x.rect().y_range().contains(p.y)).min_by(|(_, a), (_, b)| across(a).total_cmp(&across(b)));
        if let Some((piece, x)) = level {
            return At { piece, index: x.galley.cursor_from_pos(p - x.at).index.0 };
        }
        match self.pieces.iter().position(|x| x.at.y > p.y) {
            Some(piece) => At { piece, index: 0 },
            None => self.end(),
        }
    }

    fn end(&self) -> At {
        At { piece: self.pieces.len().saturating_sub(1), index: self.pieces.last().map_or(0, Piece::chars) }
    }

    /// The link whose text is under `p`, as (piece, link).
    fn link_at(&self, p: Pos2) -> Option<(usize, usize)> {
        let (i, piece) = self.pieces.iter().enumerate().find(|(_, x)| !x.links.is_empty() && x.rect().contains(p))?;
        let q = p - piece.at;
        let c = piece.galley.cursor_from_pos(q).index.0;
        // The character under `p`: the one after the nearest boundary, or the one before it.
        let under = [c, c.saturating_sub(1)].into_iter().find(|&k| {
            let (a, b) = (piece.galley.pos_from_cursor(CCursor::new(k)), piece.galley.pos_from_cursor(CCursor::new(k + 1)));
            a.min.y == b.min.y && (a.min.x..=b.min.x).contains(&q.x) && a.y_range().contains(q.y)
        })?;
        Some((i, piece.links.iter().position(|(range, ..)| range.contains(&under))?))
    }

    /// The characters of piece `i` inside the selection `(lo, hi)`.
    fn span(&self, i: usize, (lo, hi): (At, At)) -> Option<Range<usize>> {
        if i < lo.piece || i > hi.piece {
            return None;
        }
        let start = if i == lo.piece { lo.index } else { 0 };
        let end = if i == hi.piece { hi.index } else { self.pieces[i].chars() };
        (start < end).then_some(start..end)
    }

    /// The selection `(lo, hi)` as text: list markers, line breaks and a table's tabs included.
    fn copied(&self, (lo, hi): (At, At)) -> String {
        let mut out = String::new();
        for i in lo.piece..=hi.piece.min(self.pieces.len().saturating_sub(1)) {
            let piece = &self.pieces[i];
            if let Some(span) = self.span(i, (lo, hi)) {
                if span.start == 0 {
                    out += &piece.prefix;
                }
                out.extend(piece.galley.text().chars().skip(span.start).take(span.len()));
            }
            if i < hi.piece {
                out += piece.after;
            }
        }
        out
    }

    /// The word at `at`, or the one character there when it is not in a word.
    fn word(&self, at: At) -> (At, At) {
        let Some(piece) = self.pieces.get(at.piece) else { return (at, at) };
        let chars: Vec<char> = piece.galley.text().chars().collect();
        let is_word = |k: usize| chars.get(k).is_some_and(|c| egui::text_selection::text_cursor_state::is_word_char(*c));
        // The boundary nearest the pointer may be just after the word.
        let k = if !is_word(at.index) && at.index > 0 && is_word(at.index - 1) { at.index - 1 } else { at.index };
        if !is_word(k) {
            let end = (k + 1).min(chars.len());
            return (At { index: k.min(end), ..at }, At { index: end, ..at });
        }
        let start = chars[..k].iter().rposition(|c| !egui::text_selection::text_cursor_state::is_word_char(*c)).map_or(0, |p| p + 1);
        let end = chars[k..].iter().position(|c| !egui::text_selection::text_cursor_state::is_word_char(*c)).map_or(chars.len(), |p| k + p);
        (At { index: start, ..at }, At { index: end, ..at })
    }
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 22.0,
        2 => 20.0,
        3 => 18.0,
        _ => 16.5,
    }
}

/// Which body last took a click: only its selection is kept, so one Ctrl+C copies one thing.
fn selecting_id() -> egui::Id {
    egui::Id::new("body_selecting")
}

/// Draws `blocks` with room `gap.1` tall before block `gap.0`, for a control the caller puts in the
/// returned rect. Returns the link clicked, cleaned.
fn show(ui: &mut Ui, t: &Tokens, blocks: &[Block], gap: Option<(usize, f32)>) -> (Option<String>, Option<Rect>) {
    let id = ui.id().with(("body", blocks));
    let mut state = ui.data(|d| d.get_temp::<State>(id)).unwrap_or_default();
    if ui.data(|d| d.get_temp::<egui::Id>(selecting_id())) != Some(id) {
        state.anchor = None;
    }
    let width = ui.available_width();
    let laid = Laid::new(ui, t, blocks, width, gap, state.hovered);
    let (rect, response) = ui.allocate_exact_size(vec2(width, laid.height), Sense::click_and_drag());
    let origin = rect.min.to_vec2();
    let pointer = ui.ctx().pointer_interact_pos().map(|p| p - origin);
    let (pressed, down, shift) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.modifiers.shift));

    if let Some(p) = pointer
        && pressed
        && response.is_pointer_button_down_on()
    {
        let at = laid.at(p);
        if !(shift && state.anchor.is_some()) {
            state.anchor = Some(at);
        }
        state.head = at;
        state.dragging = true;
        ui.data_mut(|d| d.insert_temp(selecting_id(), id));
    } else if state.dragging
        && down
        && let Some(p) = pointer
    {
        state.head = laid.at(p);
        // Past the top or the bottom of the view, it scrolls on, faster the further the pointer is.
        let clip = ui.clip_rect();
        let y = p.y + origin.y;
        let past = if y < clip.top() { clip.top() - y } else { (clip.bottom() - y).min(0.0) };
        if past != 0.0 {
            ui.scroll_with_delta(vec2(0.0, past.clamp(-30.0, 30.0)));
            ui.ctx().request_repaint();
        }
    }
    if !down {
        state.dragging = false;
    }
    if let Some(p) = pointer {
        let at = laid.at(p);
        if response.triple_clicked() {
            let chars = laid.pieces.get(at.piece).map_or(0, Piece::chars);
            (state.anchor, state.head) = (Some(At { index: 0, ..at }), At { index: chars, ..at });
        } else if response.double_clicked() {
            let (start, end) = laid.word(at);
            (state.anchor, state.head) = (Some(start), end);
        }
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.anchor = None;
    }
    let range = state.range();
    if let Some(range) = range
        && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
        && ui.memory(|m| m.focused().is_none())
    {
        ui.ctx().copy_text(laid.copied(range));
    }

    let hovered = response.hover_pos().and_then(|p| laid.link_at(p - origin));
    if hovered != state.hovered {
        state.hovered = hovered;
        ui.ctx().request_repaint();
    }
    let link = hovered.map(|(piece, link)| {
        let (_, text, url) = &laid.pieces[piece].links[link];
        LinkInfo::new(ui.ctx(), text, url)
    });
    let mut clicked = None;
    if response.clicked() && !shift && range.is_none() {
        clicked = link.as_ref().map(|l| l.clean.clone());
    }
    // The menu stays open after the pointer leaves the link: it keeps the one right-clicked.
    if response.secondary_clicked() {
        state.menu_link = link.as_ref().map(|l| l.clean.clone());
    }
    let mut select_all = false;
    views::context_menu(&response, |ui| {
        if let Some(clean) = &state.menu_link {
            if ui.button("Open link").clicked() {
                clicked = Some(clean.clone());
            }
            if ui.button("Copy link").on_hover_text("Copied without tracking parameters").clicked() {
                ui.ctx().copy_text(clean.clone());
            }
            ui.separator();
        }
        if ui.add_enabled(range.is_some(), egui::Button::new("Copy")).clicked()
            && let Some(range) = range
        {
            ui.ctx().copy_text(laid.copied(range));
        }
        if ui.button("Select all").clicked() {
            select_all = true;
        }
    });
    if select_all {
        (state.anchor, state.head) = (Some(At::default()), laid.end());
        ui.data_mut(|d| d.insert_temp(selecting_id(), id));
    }
    match &link {
        Some(link) => {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            response.on_hover_text(&link.tip);
        }
        None if response.hovered() || state.dragging => ui.ctx().set_cursor_icon(CursorIcon::Text),
        None => {}
    }

    let painter = ui.painter();
    let clip = ui.clip_rect();
    for mark in &laid.marks {
        match mark {
            Mark::Text(at, galley) => painter.galley(*at + origin, galley.clone(), t.text_dim),
            Mark::Line(a, b, stroke) => {
                painter.line_segment([*a + origin, *b + origin], *stroke);
            }
            Mark::Box(r, fill) => {
                painter.rect_filled(r.translate(origin), CornerRadius::same(CONTROL_RADIUS), *fill);
            }
        }
    }
    let range = state.range();
    for (i, piece) in laid.pieces.iter().enumerate() {
        let r = piece.rect().translate(origin);
        if !clip.intersects(r) {
            continue;
        }
        let mut galley = piece.galley.clone();
        if let Some(span) = range.and_then(|range| laid.span(i, range)) {
            egui::text_selection::visuals::paint_text_selection(&mut galley, ui.visuals(), &CCursorRange::two(CCursor::new(span.start), CCursor::new(span.end)), None);
        }
        painter.galley(r.min, galley, t.text);
    }
    ui.data_mut(|d| d.insert_temp(id, state));
    (clicked, laid.gap.map(|g| g.translate(origin)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    /// Lays `blocks` out `width` wide in a frame of a fresh context, and hands the result to `check`.
    fn laid(blocks: &[Block], width: f32, check: impl Fn(&Laid)) {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx.run_ui(egui::RawInput::default(), |ui| check(&Laid::new(ui, &Tokens::light(), blocks, width, None, None))).drop_without_applying_deltas();
    }

    #[test]
    fn a_selection_copies_the_same_text_whichever_way_it_was_made() {
        let blocks = html::blocks("<p>First para</p><ul><li>one</li><li>two</li></ul><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>");
        laid(&blocks, 600.0, |laid| {
            let all = laid.copied((At::default(), laid.end()));
            assert_eq!(all, "First para\n\n• one\n• two\nA\tB\n1\t2");
            let from = At { piece: 0, index: 6 };
            let state = State { anchor: Some(laid.end()), head: from, ..Default::default() };
            assert_eq!(state.range(), Some((from, laid.end())), "dragged upwards, it is the same range");
            assert!(laid.copied(state.range().unwrap()).starts_with("para\n\n• one"));
        });
    }

    #[test]
    fn a_place_beside_or_between_lines_is_the_nearest_end_of_text() {
        let blocks = html::blocks("<p>short</p><p>second paragraph</p>");
        laid(&blocks, 600.0, |laid| {
            let first = &laid.pieces[0];
            assert_eq!(laid.at(pos2(590.0, first.at.y + 4.0)), At { piece: 0, index: 5 }, "right of a line: its end");
            assert_eq!(laid.at(pos2(10.0, first.rect().bottom() + 3.0)), At { piece: 1, index: 0 }, "between paragraphs: the next one's start");
            assert_eq!(laid.at(pos2(10.0, laid.height + 100.0)), laid.end(), "below everything: the end");
            assert_eq!(laid.word(At { piece: 1, index: 9 }), (At { piece: 1, index: 7 }, At { piece: 1, index: 16 }));
        });
    }

    #[test]
    fn a_link_is_found_only_over_its_own_text() {
        let blocks = html::blocks(r#"<p>see <a href="https://a.example/x">here</a></p>"#);
        laid(&blocks, 600.0, |laid| {
            let piece = &laid.pieces[0];
            let x = |k: usize| piece.galley.pos_from_cursor(CCursor::new(k)).min.x;
            let y = piece.at.y + piece.galley.size().y / 2.0;
            assert_eq!(laid.link_at(pos2(x(5) + 1.0, y)), Some((0, 0)));
            assert_eq!(laid.link_at(pos2(x(1), y)), None, "the words before it");
            assert_eq!(laid.link_at(pos2(x(8) + 40.0, y)), None, "the room after it");
        });
    }
}
