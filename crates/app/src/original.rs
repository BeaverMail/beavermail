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
use blitz_traits::net::NetProvider;
use eframe::egui::{self, ColorImage, Pos2, TextureHandle, TextureOptions, Vec2};

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::remote::Net;

/// Inputs beyond this are cut, like in the simplified view: a huge newsletter must not freeze the window.
const MAX_INPUT: usize = 400_000;
/// Taller pages are cut to what a texture can hold.
const MAX_HEIGHT_PX: u32 = 8192;

fn pixels(width: f32, scale: f32) -> u32 {
    (width * scale).round().max(1.0) as u32
}

/// Lays the document out for `pixels` of width and draws all of it; returns the picture and its size in points.
fn draw(ctx: &egui::Context, doc: &mut HtmlDocument, pixels: u32, scale: f32) -> (TextureHandle, Vec2) {
    doc.set_viewport(Viewport::new(pixels, 800, scale, ColorScheme::Light));
    doc.resolve(0.0);
    let height = (doc.root_element().scroll_height() * scale).ceil().clamp(1.0, MAX_HEIGHT_PX as f32) as u32;
    doc.set_viewport(Viewport::new(pixels, height, scale, ColorScheme::Light));
    doc.resolve(0.0);
    let rgba = render_to_buffer::<VelloCpuImageRenderer, _>(|scene| blitz_paint::paint_scene(scene, doc, scale as f64, pixels, height, 0, 0), pixels, height);
    let image = ColorImage::from_rgba_premultiplied([pixels as usize, height as usize], &rgba);
    (ctx.load_texture("original-html", image, TextureOptions::LINEAR), Vec2::new(pixels as f32 / scale, height as f32 / scale))
}

/// A message laid out for one width and drawn.
pub struct Page {
    doc: HtmlDocument,
    texture: TextureHandle,
    /// The width this was drawn for, in points, and the pixels per point.
    width: f32,
    scale: f32,
    /// Size on screen, in points.
    pub size: Vec2,
    /// Remote content is being fetched, and how many files had arrived at the last redraw.
    net: Option<Arc<Net>>,
    seen: usize,
    drawn: Instant,
    ctx: egui::Context,
}

impl Page {
    /// `remote`: the user agreed to load what the message links to, except `trackers`.
    pub fn new(ctx: &egui::Context, html: &str, width: f32, scale: f32, remote: bool, trackers: &[String]) -> Option<Self> {
        let cut = (0..=MAX_INPUT.min(html.len())).rev().find(|i| html.is_char_boundary(*i)).unwrap_or(0);
        let html = &html[..cut];
        let pixels = pixels(width, scale);
        let net = remote.then(|| {
            let ctx = ctx.clone();
            Net::new(trackers, move || ctx.request_repaint())
        });
        let config = DocumentConfig {
            viewport: Some(Viewport::new(pixels, 800, scale, ColorScheme::Light)),
            net_provider: net.clone().map(|n| n as Arc<dyn NetProvider>),
            ..Default::default()
        };
        let mut doc = HtmlDocument::from_html(html, config);
        let (texture, size) = draw(ctx, &mut doc, pixels, scale);
        Some(Self { doc, texture, width, scale, size, net, seen: 0, drawn: Instant::now(), ctx: ctx.clone() })
    }

    /// Whether this was drawn for about this width (a few points of difference are not worth a redraw)
    /// and with the same permission to load remote content.
    pub fn fits(&self, width: f32, scale: f32, remote: bool) -> bool {
        scale == self.scale && (self.width - width).abs() < 24.0 && self.net.is_some() == remote
    }

    /// Draws again when files have arrived since the last time, at most a few times a second.
    fn refresh(&mut self) {
        let Some(net) = &self.net else { return };
        let done = net.done.load(Ordering::Relaxed);
        if done == self.seen {
            return;
        }
        let wait = Duration::from_millis(300);
        if self.drawn.elapsed() < wait {
            self.ctx.request_repaint_after(wait);
            return;
        }
        self.seen = done;
        self.drawn = Instant::now();
        self.doc.handle_messages();
        (self.texture, self.size) = draw(&self.ctx, &mut self.doc, pixels(self.width, self.scale), self.scale);
    }

    /// Draws the page on a white sheet; returns the link under a click, if any.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<String> {
        self.refresh();
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
            let page = Page::new(&ctx, HTML, 400.0, scale, false, &[]).unwrap();
            assert!(page.size.x > 399.0 && page.size.y > 40.0 && page.size.y < 400.0, "{:?}", page.size);
            assert_eq!(page.link_at(Vec2::new(20.0, 20.0)).as_deref(), Some("https://a.example/x"), "scale {scale}");
            assert_eq!(page.link_at(Vec2::new(20.0, 90.0)), None, "scale {scale}");
        }
    }

    #[test]
    fn remote_images_are_fetched_only_after_consent_and_without_identifying_headers() {
        use std::io::{Read, Write};
        const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        crate::remote::TEST_PORT.store(addr.port(), Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 2048];
                let n = stream.read(&mut request).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&request[..n]).to_lowercase());
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", PNG.len()).as_bytes());
                let _ = stream.write_all(PNG);
            }
        });
        let html = format!(r#"<body style="margin:0"><img src="http://{addr}/t.gif" width="1" height="1"><img src="http://{addr}/p.png" width="40" height="40"></body>"#);
        let trackers = crate::html::trackers(&html);
        let ctx = egui::Context::default();
        let blocked = Page::new(&ctx, &html, 200.0, 1.0, false, &trackers).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(rx.try_recv().is_err(), "nothing is requested without consent");
        drop(blocked);

        let mut page = Page::new(&ctx, &html, 200.0, 1.0, true, &trackers).unwrap();
        let request = rx.recv_timeout(Duration::from_secs(5)).expect("the image is requested after consent");
        assert!(request.starts_with("get /p.png") && !request.contains("referer") && !request.contains("cookie"), "{request}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(rx.try_recv().is_err(), "the tracking pixel is never requested, even with consent");
        for _ in 0..50 {
            if page.net.as_ref().unwrap().done.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(350));
        page.refresh();
        assert_eq!(page.seen, 1, "the page took the arrival into account");
    }
}
