//! Message bodies as blocks of styled runs: the "simplified HTML" and "plain text" display modes.
//!
//! The HTML is untrusted and is never rendered as HTML: it is walked once and reduced to text
//! structure (headings, paragraphs, lists, quotes, links). Scripts, styles, forms' behaviour and
//! images are dropped, so nothing is fetched and nothing can track the reader.

use scraper::{ElementRef, Html, Node};

/// Input beyond this is cut: a huge newsletter must not freeze the window.
const MAX_INPUT: usize = 400_000;
const MAX_BLOCKS: usize = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    Text { text: String, bold: bool, italic: bool, code: bool },
    Link { text: String, url: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A paragraph: space after it.
    Para,
    /// A line inside a flow (after a `<br>`, a table cell): no space after it.
    Line,
    Heading(u8),
    /// A list item, with the marker already chosen ("•" or "3.").
    Item,
    Quote,
    Pre,
    Rule,
    /// A table of data, in `rows`; `header`: the first row names the columns.
    Table { header: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: Kind,
    /// Nesting depth of lists and quotes.
    pub depth: u8,
    pub marker: Option<String>,
    pub runs: Vec<Run>,
    /// A table's rows of cells; empty for any other kind.
    pub rows: Vec<Vec<Vec<Run>>>,
}

#[derive(Clone, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    link: Option<String>,
}

struct Walker {
    blocks: Vec<Block>,
    runs: Vec<Run>,
    kind: Kind,
    depth: u8,
    marker: Option<String>,
    /// Counters of the open lists: `Some(n)` for ordered, `None` for bullets.
    lists: Vec<Option<u32>>,
    pre: bool,
}

/// Whether showing the message as designed would fetch anything: an image, style or font on a server.
pub fn has_remote(html: &str) -> bool {
    let remote = |v: &str| {
        let v = v.trim_start().to_ascii_lowercase();
        v.starts_with("http://") || v.starts_with("https://") || v.starts_with("//")
    };
    let mut end = html.len().min(MAX_INPUT);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let document = Html::parse_document(&html[..end]);
    document.tree.nodes().any(|n| match n.value() {
        Node::Element(e) => ["src", "srcset", "background", "poster"].iter().any(|a| e.attr(a).is_some_and(remote)) || (e.name() == "link" && e.attr("href").is_some_and(remote)) || e.attr("style").is_some_and(|s| s.to_ascii_lowercase().contains("url(")),
        Node::Text(t) => t.text.to_ascii_lowercase().contains("url(") || t.text.contains("@import"),
        _ => false,
    })
}

/// The remote images of the HTML that only report the message was opened: pixel-sized or hidden
/// images, and known open-tracking addresses. As absolute URLs, to leave out when loading images.
pub fn trackers(html: &str) -> Vec<String> {
    let mut end = html.len().min(MAX_INPUT);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let document = Html::parse_document(&html[..end]);
    let mut out: Vec<String> = Vec::new();
    for node in document.tree.nodes() {
        let Node::Element(e) = node.value() else { continue };
        if e.name() != "img" {
            continue;
        }
        let Some(src) = e.attr("src").map(str::trim) else { continue };
        let src = if src.starts_with("//") { format!("https:{src}") } else { src.to_owned() };
        let Ok(url) = blitz_traits::net::Url::parse(&src) else { continue };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let style: String = e.attr("style").unwrap_or("").to_ascii_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        // A size of at most 2: "1", "1px", "0".
        let tiny = |v: &str| v.trim().trim_end_matches("px").parse::<f32>().is_ok_and(|n| n <= 2.0);
        let styled = |prop: &str| style.split(';').filter_map(|d| d.strip_prefix(prop)).next_back().is_some_and(tiny);
        let pixel = e.attr("width").is_some_and(tiny) || e.attr("height").is_some_and(tiny) || styled("width:") || styled("height:");
        let hidden = e.attr("hidden").is_some() || style.contains("display:none") || style.contains("visibility:hidden") || style.contains("opacity:0");
        if (pixel || hidden || crate::remote::tracking(&url)) && !out.contains(&url.to_string()) {
            out.push(url.to_string());
        }
    }
    out
}

/// How many letters and digits the HTML hides from the reader (`display: none`, zero size and the
/// like). A short hidden preheader is normal in newsletters; a long one is meant for spam filters.
pub fn hidden_text(html: &str) -> usize {
    /// `hidden`: nothing below can be seen. `tiny`: font size zero, which a descendant can set back,
    /// as layout templates do (zero on the container removes the gaps between inline blocks).
    fn walk(el: ElementRef<'_>, hidden: bool, tiny: bool) -> usize {
        let style: String = el.value().attr("style").unwrap_or("").to_ascii_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        // The number in front of the unit ("0", "0px", "16px"), if the property is set.
        let number = |prop: &str| style.split(';').filter_map(|d| d.strip_prefix(prop)).next_back().and_then(|v| v.split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?.parse::<f32>().ok());
        let hidden = hidden || el.value().attr("hidden").is_some() || style.contains("display:none") || style.contains("visibility:hidden") || number("opacity:") == Some(0.0);
        let tiny = number("font-size:").map_or(tiny, |size| size == 0.0);
        el.children()
            .map(|child| match child.value() {
                Node::Text(t) if hidden || tiny => t.chars().filter(|c| c.is_alphanumeric()).count(),
                Node::Element(_) => ElementRef::wrap(child).map_or(0, |c| walk(c, hidden, tiny)),
                _ => 0,
            })
            .sum()
    }
    let mut end = html.len().min(MAX_INPUT);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    walk(Html::parse_document(&html[..end]).root_element(), false, false)
}

/// The HTML reduced to blocks.
pub fn blocks(html: &str) -> Vec<Block> {
    let mut end = html.len().min(MAX_INPUT);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let html = &html[..end];
    let document = Html::parse_document(html);
    let mut walker = Walker { blocks: Vec::new(), runs: Vec::new(), kind: Kind::Para, depth: 0, marker: None, lists: Vec::new(), pre: false };
    walker.element(document.root_element(), &Style::default());
    walker.flush();
    walker.blocks
}

impl Walker {
    fn flush(&mut self) {
        let mut runs = std::mem::take(&mut self.runs);
        if !self.pre {
            trim_runs(&mut runs);
        }
        let marker = self.marker.take();
        if runs.is_empty() && self.kind != Kind::Rule && marker.is_none() {
            return;
        }
        if self.blocks.len() < MAX_BLOCKS {
            self.blocks.push(Block { kind: self.kind, depth: self.depth, marker, runs, rows: Vec::new() });
        }
    }

    /// Ends the current block and starts a new one of `kind`.
    fn start(&mut self, kind: Kind) {
        self.flush();
        self.kind = kind;
    }

    fn text(&mut self, raw: &str, style: &Style) {
        let text = if self.pre { raw.to_owned() } else { collapse(raw, matches!(self.runs.last(), Some(Run::Text { text, .. }) if text.ends_with(' '))) };
        if text.is_empty() {
            return;
        }
        match &style.link {
            Some(url) => self.runs.push(Run::Link { text, url: url.clone() }),
            None => self.runs.push(Run::Text { text, bold: style.bold, italic: style.italic, code: style.code || self.pre }),
        }
    }

    fn element(&mut self, el: ElementRef<'_>, style: &Style) {
        let name = el.value().name();
        if matches!(name, "script" | "style" | "head" | "title" | "meta" | "link" | "noscript" | "template" | "svg" | "iframe" | "object" | "embed" | "canvas" | "audio" | "video") {
            return;
        }
        let mut style = style.clone();
        let (outer_kind, outer_depth, outer_pre) = (self.kind, self.depth, self.pre);
        if name == "table"
            && let Some((header, rows)) = data_table(el, &style)
        {
            self.start(Kind::Para);
            if self.blocks.len() < MAX_BLOCKS {
                self.blocks.push(Block { kind: Kind::Table { header }, depth: self.depth, marker: None, runs: Vec::new(), rows });
            }
            return;
        }
        match name {
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside" | "nav" | "table" | "tbody" | "thead" | "tfoot" | "tr" | "address" | "figure" | "dl" => self.start(Kind::Para),
            "td" | "th" | "dd" | "dt" | "figcaption" | "caption" => self.start(Kind::Line),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.start(Kind::Heading(name.as_bytes()[1] - b'0'));
                style.bold = true;
            }
            "blockquote" => {
                self.depth += 1;
                self.start(Kind::Quote);
            }
            "pre" => {
                self.start(Kind::Pre);
                self.pre = true;
            }
            "ul" | "ol" => {
                self.start(Kind::Para);
                self.lists.push((name == "ol").then_some(0));
                self.depth += 1;
            }
            "li" => {
                self.start(Kind::Item);
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{n}.")
                    }
                    _ => "•".to_owned(),
                });
            }
            "hr" => {
                self.start(Kind::Rule);
                self.flush();
                self.kind = Kind::Para;
                return;
            }
            "br" => {
                let kind = self.kind;
                self.start(Kind::Line);
                self.kind = if matches!(kind, Kind::Heading(_) | Kind::Item | Kind::Quote | Kind::Pre) { kind } else { Kind::Line };
                self.marker = None;
                return;
            }
            "b" | "strong" => style.bold = true,
            "i" | "em" | "cite" | "dfn" => style.italic = true,
            "code" | "tt" | "kbd" | "samp" => style.code = true,
            "a" => {
                if let Some(href) = el.value().attr("href").map(str::trim).filter(|h| is_safe_link(h)) {
                    style.link = Some(href.to_owned());
                }
            }
            "img" => {
                // Images are never loaded. Alt text is kept, since it is often the only content.
                if let Some(alt) = el.value().attr("alt").map(str::trim).filter(|a| !a.is_empty()) {
                    self.text(&format!("[{alt}]"), &Style { italic: true, ..Style::default() });
                }
                return;
            }
            _ => {}
        }
        for child in el.children() {
            match child.value() {
                Node::Text(text) => self.text(text, &style),
                Node::Element(_) => {
                    if let Some(child) = ElementRef::wrap(child) {
                        self.element(child, &style);
                    }
                }
                _ => {}
            }
        }
        if matches!(name, "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside" | "nav" | "table" | "tbody" | "thead" | "tfoot" | "tr" | "address" | "figure" | "dl" | "td" | "th" | "dd" | "dt" | "figcaption" | "caption" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "pre" | "ul" | "ol" | "li") {
            self.flush();
        }
        if matches!(name, "ul" | "ol") {
            self.lists.pop();
        }
        self.kind = outer_kind;
        self.depth = outer_depth;
        self.pre = outer_pre;
        if matches!(name, "td" | "th" | "br") {
            self.kind = Kind::Line;
        }
        if !matches!(name, "td" | "th" | "br") {
            self.kind = if matches!(outer_kind, Kind::Heading(_) | Kind::Item | Kind::Quote | Kind::Pre) { Kind::Para } else { outer_kind };
        }
    }
}

/// A table holding data rather than laying out the message, as its rows of cells and whether the
/// first row is a header: at least two rows of two cells or more, short cells, no table inside.
/// Newsletters lay out with tables; those stay flowing text.
fn data_table(table: ElementRef<'_>, style: &Style) -> Option<(bool, Vec<Vec<Vec<Run>>>)> {
    const MAX_CELL: usize = 120;
    const MAX_COLUMNS: usize = 8;
    if table.value().attr("role").is_some_and(|r| r.eq_ignore_ascii_case("presentation") || r.eq_ignore_ascii_case("none")) {
        return None;
    }
    let elements = || table.descendants().skip(1).filter_map(ElementRef::wrap);
    if elements().any(|e| e.value().name() == "table") {
        return None;
    }
    let mut header = false;
    let mut rows = Vec::new();
    for (i, tr) in elements().filter(|e| e.value().name() == "tr").enumerate() {
        let cells: Vec<ElementRef<'_>> = tr.children().filter_map(ElementRef::wrap).filter(|c| matches!(c.value().name(), "td" | "th")).collect();
        if cells.len() > MAX_COLUMNS || cells.iter().any(|c| c.text().map(|t| t.chars().count()).sum::<usize>() > MAX_CELL) {
            return None;
        }
        let row: Vec<Vec<Run>> = cells.iter().map(|c| cell_runs(*c, style)).collect();
        if row.iter().all(Vec::is_empty) {
            continue;
        }
        if rows.is_empty() {
            header = i == 0 && (cells.iter().all(|c| c.value().name() == "th") || tr.parent().and_then(ElementRef::wrap).is_some_and(|p| p.value().name() == "thead"));
        }
        rows.push(row);
    }
    (rows.iter().filter(|r| r.len() >= 2).count() >= 2).then_some((header, rows))
}

/// A cell's content on one line: its paragraphs and lines joined by spaces.
fn cell_runs(cell: ElementRef<'_>, style: &Style) -> Vec<Run> {
    let style = Style { bold: style.bold || cell.value().name() == "th", ..style.clone() };
    let mut walker = Walker { blocks: Vec::new(), runs: Vec::new(), kind: Kind::Line, depth: 0, marker: None, lists: Vec::new(), pre: false };
    walker.element(cell, &style);
    walker.flush();
    let mut runs = Vec::new();
    for block in walker.blocks {
        if !runs.is_empty() {
            runs.push(Run::Text { text: " ".into(), bold: false, italic: false, code: false });
        }
        runs.extend(block.runs);
    }
    runs
}

/// Only web and mail links are kept; `javascript:`, `data:` and the like become plain text.
fn is_safe_link(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// HTML whitespace rules: any run of whitespace is one space, and none right after a space.
fn collapse(raw: &str, after_space: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = after_space;
    for c in raw.chars() {
        if c.is_whitespace() || c == '\u{a0}' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out
}

fn trim_runs(runs: &mut Vec<Run>) {
    if let Some(Run::Text { text, .. } | Run::Link { text, .. }) = runs.first_mut() {
        *text = text.trim_start().to_owned();
    }
    if let Some(Run::Text { text, .. } | Run::Link { text, .. }) = runs.last_mut() {
        *text = text.trim_end().to_owned();
    }
    runs.retain(|r| !matches!(r, Run::Text { text, .. } | Run::Link { text, .. } if text.is_empty()));
}

/// A plain-text message as blocks: one per line, links made clickable, quoted lines grouped.
pub fn plain_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    for line in text.lines().take(MAX_BLOCKS) {
        let (quoted, body) = quote_depth(line);
        let mut runs = Vec::new();
        let mut rest = body;
        while let Some(start) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
            let end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |e| start + e);
            let url = rest[start..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '>', '"', '\'']);
            if start > 0 {
                runs.push(Run::Text { text: rest[..start].to_owned(), bold: false, italic: false, code: false });
            }
            runs.push(Run::Link { text: url.to_owned(), url: url.to_owned() });
            rest = &rest[start + url.len()..];
        }
        if !rest.is_empty() {
            runs.push(Run::Text { text: rest.to_owned(), bold: false, italic: false, code: false });
        }
        let kind = if line.trim().is_empty() { Kind::Para } else if quoted > 0 { Kind::Quote } else { Kind::Line };
        blocks.push(Block { kind, depth: quoted, marker: None, runs, rows: Vec::new() });
    }
    blocks
}

/// How many `>` levels start a line, and the rest of the line.
fn quote_depth(line: &str) -> (u8, &str) {
    let mut depth = 0;
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        match trimmed.strip_prefix('>') {
            Some(after) => {
                depth += 1;
                rest = after.strip_prefix(' ').unwrap_or(after);
            }
            None => return (depth, rest),
        }
    }
}

/// The address without scheme, credentials, port or `www.`, lower case.
pub fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = authority.split(':').next().unwrap_or("").to_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_owned()
}

/// A link whose visible text names a different site than the one it goes to.
pub fn looks_misleading(text: &str, url: &str) -> bool {
    let text = text.trim();
    let named = text.starts_with("http://") || text.starts_with("https://") || (text.contains('.') && !text.contains(' ') && !text.contains('@'));
    named && !url.to_ascii_lowercase().starts_with("mailto:") && !crate::sender::same_site(&host(text), &host(url))
}

/// The link without known tracking parameters.
pub fn clean_url(url: &str) -> String {
    const TRACKING: [&str; 17] = [
        "fbclid", "gclid", "dclid", "msclkid", "yclid", "mc_eid", "mc_cid", "mkt_tok", "igshid", "_hsenc", "_hsmi", "hsctatracking", "vero_id", "oly_enc_id", "oly_anon_id", "s_cid", "ref_src",
    ];
    let (before_fragment, fragment) = url.split_once('#').map_or((url, None), |(a, b)| (a, Some(b)));
    let Some((base, query)) = before_fragment.split_once('?') else { return url.to_owned() };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
            !(key.starts_with("utm_") || TRACKING.contains(&key.as_str()))
        })
        .collect();
    let mut out = base.to_owned();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_hidden_images_and_known_trackers_are_found_but_pictures_are_not() {
        let html = r#"<img src="https://t.example/o.gif" width="1" height="1">
            <img src="//t.example/p.gif" style="display: none">
            <img src="https://x.us5.list-manage.com/track/open.php?u=1">
            <img src="https://shop.example/logo.png" width="120">
            <img src="cid:part1" width="1">"#;
        assert_eq!(trackers(html), ["https://t.example/o.gif", "https://t.example/p.gif", "https://x.us5.list-manage.com/track/open.php?u=1"]);
    }

    fn texts(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .map(|b| {
                let text: String = b.runs.iter().map(|r| match r {
                    Run::Text { text, .. } | Run::Link { text, .. } => text.as_str(),
                }).collect();
                match &b.marker {
                    Some(m) => format!("{m} {text}"),
                    None => text,
                }
            })
            .collect()
    }

    #[test]
    fn structure_survives_and_scripts_and_styles_do_not() {
        let html = "<html><head><title>x</title><style>p{color:red}</style></head><body>\
            <h1>Title</h1><p>Hello   <b>big</b>\n world</p><script>alert(1)</script>\
            <ul><li>one</li><li>two</li></ul><ol><li>a</li><li>b</li></ol></body></html>";
        assert_eq!(texts(&blocks(html)), ["Title", "Hello big world", "• one", "• two", "1. a", "2. b"]);
    }

    #[test]
    fn images_are_dropped_but_alt_text_stays() {
        let html = "<p>Hi<img src='http://track.example/p.gif' width=1 height=1><img src='x' alt='Logo'></p>";
        assert_eq!(texts(&blocks(html)), ["Hi[Logo]"]);
    }

    #[test]
    fn only_web_and_mail_links_are_links() {
        let html = "<p><a href='https://a.example/x'>site</a> <a href='javascript:alert(1)'>bad</a> <a href='mailto:me@x.org'>me</a></p>";
        let b = blocks(html);
        let links: Vec<&str> = b[0].runs.iter().filter_map(|r| match r { Run::Link { url, .. } => Some(url.as_str()), _ => None }).collect();
        assert_eq!(links, ["https://a.example/x", "mailto:me@x.org"]);
        assert!(texts(&b)[0].contains("bad"));
    }

    #[test]
    fn line_breaks_and_quotes_keep_their_shape() {
        let b = blocks("<p>one<br>two</p><blockquote>quoted<blockquote>deeper</blockquote></blockquote>");
        assert_eq!(texts(&b), ["one", "two", "quoted", "deeper"]);
        assert_eq!(b[2].kind, Kind::Quote);
        assert_eq!(b[3].depth, 2);
    }

    #[test]
    fn plain_text_gets_clickable_links_and_quote_levels() {
        let b = plain_blocks("See https://a.example/x?y=1. Done\n> quoted\n>> deeper");
        assert_eq!(b[0].runs.len(), 3);
        assert!(matches!(&b[0].runs[1], Run::Link { url, .. } if url == "https://a.example/x?y=1"));
        assert_eq!((b[1].kind, b[1].depth), (Kind::Quote, 1));
        assert_eq!(b[2].depth, 2);
    }

    #[test]
    fn tracking_parameters_are_removed_and_the_rest_kept() {
        assert_eq!(clean_url("https://x.example/p?id=7&utm_source=n&fbclid=abc#top"), "https://x.example/p?id=7#top");
        assert_eq!(clean_url("https://x.example/p?utm_medium=a&gclid=b"), "https://x.example/p");
        assert_eq!(clean_url("https://x.example/p"), "https://x.example/p");
    }

    #[test]
    fn a_link_that_names_another_site_is_flagged() {
        assert!(looks_misleading("www.bank.com", "https://evil.example/login"));
        assert!(looks_misleading("https://bank.com/a", "https://evil.example/"));
        assert!(!looks_misleading("https://www.bank.com/a", "https://bank.com/b"));
        assert!(!looks_misleading("Click here", "https://evil.example/"));
        assert!(!looks_misleading("www.scaleway.com", "https://links.scaleway.com/c/1"), "a subdomain of the site named");
    }

    #[test]
    fn hidden_text_is_measured_once() {
        let html = r#"<p>shown</p><div style="display: none">abc <span style="font-size:16px">de</span></div><span style="FONT-SIZE: 0px">fg</span><i hidden>h</i><b style="font-size:0.9em">shown</b><b style="opacity:0.5">shown</b><div style="opacity:0">i&zwnj; &zwnj;</div>"#;
        assert_eq!(hidden_text(html), 3 + 2 + 2 + 1 + 1, "only letters and digits count");
        let layout = r#"<td style="font-size:0px"><div style="font-size:16px">Hello Ada, your invoice</div></td>"#;
        assert_eq!(hidden_text(layout), 0, "a zero-size container whose text sets its own size back is visible");
    }

    #[test]
    fn a_table_of_data_keeps_its_rows_and_a_layout_table_flows() {
        let invoice = "<p>Your order</p><table><thead><tr><th>Item</th><th>Qty</th><th>Price</th></tr></thead>\
            <tr><td><a href='https://shop.example/a'>Lamp</a></td><td>2</td><td>40 €</td></tr><tr><td></td><td></td></tr>\
            <tr><td colspan=2>Total</td><td><b>80 €</b></td></tr></table><p>Thanks</p>";
        let b = blocks(invoice);
        assert_eq!(b.len(), 3);
        assert_eq!(b[1].kind, Kind::Table { header: true });
        assert_eq!(b[1].rows.len(), 3, "the empty row is dropped");
        assert!(matches!(&b[1].rows[0][0][0], Run::Text { text, bold: true, .. } if text == "Item"));
        assert!(matches!(&b[1].rows[1][0][0], Run::Link { text, .. } if text == "Lamp"));
        assert_eq!(texts(&b), ["Your order", "", "Thanks"]);

        let layout = "<table><tr><td><table><tr><td>Logo</td><td>Menu</td></tr><tr><td>a</td><td>b</td></tr></table></td></tr></table>";
        assert_eq!(blocks(layout).iter().filter(|b| matches!(b.kind, Kind::Table { .. })).count(), 1, "the outer one holds a table, the inner one is data");
        let story = format!("<table><tr><td>{}</td><td>x</td></tr><tr><td>a</td><td>b</td></tr></table>", "long text ".repeat(20));
        assert!(blocks(&story).iter().all(|b| !matches!(b.kind, Kind::Table { .. })), "long cells are prose laid out in columns");
        assert!(blocks("<table><tr><td>a</td><td>b</td></tr></table>").iter().all(|b| !matches!(b.kind, Kind::Table { .. })), "one row is a layout");
    }

    #[test]
    fn huge_input_is_cut_instead_of_freezing() {
        let html = format!("<p>{}</p>", "x ".repeat(MAX_INPUT));
        assert!(blocks(&html).len() <= MAX_BLOCKS);
    }

    #[test]
    fn remote_content_is_detected_but_links_and_inline_images_are_not() {
        assert!(has_remote(r#"<img src="https://t.example/p.gif">"#));
        assert!(has_remote(r#"<body background="//cdn.example/bg.png">hi"#));
        assert!(has_remote(r#"<p style="background:url('http://x.example/a.png')">hi</p>"#));
        assert!(has_remote(r#"<link rel="stylesheet" href="https://x.example/a.css">"#));
        assert!(has_remote("<style>@import url(https://x.example/a.css);</style>"));
        assert!(!has_remote(r#"<p>hi <a href="https://x.example">link</a></p><img src="data:image/png;base64,AAAA"><img src="cid:logo">"#));
    }
}
