// SPDX-License-Identifier: AGPL-3.0-only

//! The "original HTML" display mode: the message as its sender styled it, drawn by Blitz (a Rust
//! HTML/CSS engine, no browser, no scripts) into a picture.
//!
//! Nothing is fetched: the document has no network provider, so images, fonts, stylesheets and
//! frames that point at a server are simply not loaded and nothing can track the reader.

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};
use eframe::egui::{self, ColorImage, Pos2, TextureHandle, TextureOptions, Vec2};

/// Inputs beyond this are cut, like in the simplified view: a huge newsletter must not freeze the window.
const MAX_INPUT: usize = 400_000;
/// Taller pages are cut to what a texture can hold.
const MAX_HEIGHT_PX: u32 = 8192;

/// A message laid out for one width and drawn once.
pub struct Page {
    doc: HtmlDocument,
    texture: TextureHandle,
    /// The width this was drawn for, in points, and the pixels per point.
    width: f32,
    scale: f32,
    /// Size on screen, in points.
    pub size: Vec2,
}

impl Page {
    pub fn new(ctx: &egui::Context, html: &str, width: f32, scale: f32) -> Option<Self> {
        let cut = (0..=MAX_INPUT.min(html.len())).rev().find(|i| html.is_char_boundary(*i)).unwrap_or(0);
        let html = &html[..cut];
        let pixels = (width * scale).round().max(1.0) as u32;
        let viewport = Viewport::new(pixels, 800, scale, ColorScheme::Light);
        let mut doc = HtmlDocument::from_html(html, DocumentConfig { viewport: Some(viewport), ..Default::default() });
        doc.resolve(0.0);
        let height = (doc.root_element().scroll_height() * scale).ceil().clamp(1.0, MAX_HEIGHT_PX as f32) as u32;
        doc.set_viewport(Viewport::new(pixels, height, scale, ColorScheme::Light));
        doc.resolve(0.0);
        let rgba = render_to_buffer::<VelloCpuImageRenderer, _>(|scene| blitz_paint::paint_scene(scene, &mut doc, scale as f64, pixels, height, 0, 0), pixels, height);
        let image = ColorImage::from_rgba_premultiplied([pixels as usize, height as usize], &rgba);
        let texture = ctx.load_texture("original-html", image, TextureOptions::LINEAR);
        Some(Self { doc, texture, width, scale, size: Vec2::new(pixels as f32 / scale, height as f32 / scale) })
    }

    /// Whether this was drawn for about this width (a few points of difference are not worth a redraw).
    pub fn fits(&self, width: f32, scale: f32) -> bool {
        scale == self.scale && (self.width - width).abs() < 24.0
    }

    /// Draws the page on a white sheet; returns the link under a click, if any.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let (rect, response) = ui.allocate_exact_size(self.size, egui::Sense::click());
        ui.painter().rect_filled(rect, 0.0, egui::Color32::WHITE);
        ui.painter().image(self.texture.id(), rect, egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), egui::Color32::WHITE);
        let at = response.interact_pointer_pos().or(response.hover_pos())? - rect.min;
        let href = self.link_at(at);
        if href.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() { href } else { None }
    }

    /// The target of the link at `at` (points from the page's top left).
    pub fn link_at(&self, at: Vec2) -> Option<String> {
        let mut id = self.doc.element_from_point(at.x, at.y)?;
        loop {
            let node = self.doc.get_node(id)?;
            if let Some(href) = node.element_data().filter(|e| &*e.name.local == "a").and_then(|e| e.attr(blitz_dom::local_name!("href"))) {
                return Some(href.to_owned());
            }
            id = node.parent?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = r#"<html><body style="margin:0;background:#fff"><p style="margin:0;font-size:40px"><a href="https://a.example/x">link text here</a></p><p style="color:red">below</p><img src="http://tracker.example/p.gif"></body></html>"#;

    #[test]
    fn a_page_is_drawn_without_fetching_and_its_links_are_found_at_any_scale() {
        let ctx = egui::Context::default();
        for scale in [1.0, 2.0] {
            let page = Page::new(&ctx, HTML, 400.0, scale).unwrap();
            assert!(page.size.x > 399.0 && page.size.y > 40.0 && page.size.y < 400.0, "{:?}", page.size);
            assert_eq!(page.link_at(Vec2::new(20.0, 20.0)).as_deref(), Some("https://a.example/x"), "scale {scale}");
            assert_eq!(page.link_at(Vec2::new(20.0, 90.0)), None, "scale {scale}");
        }
    }
}
