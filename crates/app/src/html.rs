//! Message bodies as blocks of styled runs: the "simplified HTML" and "plain text" display modes.
//!
//! The HTML is untrusted and is never rendered as HTML: it is walked once and reduced to text
//! structure (headings, paragraphs, lists, quotes, links). Scripts, styles, forms' behaviour and
//! images are dropped, so nothing is fetched and nothing can track the reader.

use scraper::{ElementRef, Html, Node};

/// Input beyond this is cut: a huge newsletter must not freeze the window.
const MAX_INPUT: usize = 400_000;
const MAX_BLOCKS: usize = 3_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Run {
    Text { text: String, bold: bool, italic: bool, code: bool, strike: bool },
    Link { text: String, url: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Block {
    pub kind: Kind,
    /// Nesting depth of lists and quotes.
    pub depth: u8,
    pub marker: Option<String>,
    /// A line of a list item after a line break: aligned with the item's text, without its marker.
    pub continued: bool,
    pub runs: Vec<Run>,
    /// A table's rows of cells; empty for any other kind.
    pub rows: Vec<Vec<Vec<Run>>>,
}

#[derive(Clone, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    /// Struck through: `<del>`, `<s>`, Markdown's `~~`.
    strike: bool,
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
    /// The block being gathered continues a list item after a line break.
    continued: bool,
    /// How many quotes the walker is inside: their paragraphs are quoted text.
    quoted: u8,
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
    let mut walker = Walker { blocks: Vec::new(), runs: Vec::new(), kind: Kind::Para, depth: 0, marker: None, lists: Vec::new(), pre: false, continued: false, quoted: 0 };
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
        // A paragraph inside a quote (as Markdown and most mail write them) is shown as quoted.
        let kind = if self.quoted > 0 && matches!(self.kind, Kind::Para | Kind::Line) { Kind::Quote } else { self.kind };
        if self.blocks.len() < MAX_BLOCKS {
            self.blocks.push(Block { kind, depth: self.depth, marker, continued: std::mem::take(&mut self.continued), runs, rows: Vec::new() });
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
            None => self.runs.push(Run::Text { text, bold: style.bold, italic: style.italic, code: style.code || self.pre, strike: style.strike }),
        }
    }

    fn element(&mut self, el: ElementRef<'_>, style: &Style) {
        let name = el.value().name();
        if matches!(name, "script" | "style" | "head" | "title" | "meta" | "link" | "noscript" | "template" | "svg" | "iframe" | "object" | "embed" | "canvas" | "audio" | "video") {
            return;
        }
        let mut style = style.clone();
        let (outer_kind, outer_depth, outer_pre, outer_quoted) = (self.kind, self.depth, self.pre, self.quoted);
        if name == "table"
            && let Some((header, rows)) = data_table(el, &style)
        {
            self.start(Kind::Para);
            if self.blocks.len() < MAX_BLOCKS {
                self.blocks.push(Block { kind: Kind::Table { header }, depth: self.depth, marker: None, continued: false, runs: Vec::new(), rows });
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
                self.quoted += 1;
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
                let (kind, marker) = (self.kind, self.marker.clone());
                // A paragraph's room comes after its last line, not after each line that breaks.
                if kind == Kind::Para {
                    self.kind = Kind::Line;
                }
                self.start(Kind::Line);
                self.kind = if matches!(kind, Kind::Heading(_) | Kind::Item | Kind::Quote | Kind::Pre | Kind::Para) { kind } else { Kind::Line };
                // The next line of a list item stays in it: same place, no marker of its own.
                self.continued = kind == Kind::Item;
                self.marker = if self.continued { marker.or_else(|| self.blocks.last().and_then(|b| b.marker.clone())) } else { None };
                return;
            }
            "b" | "strong" => style.bold = true,
            "i" | "em" | "cite" | "dfn" => style.italic = true,
            "code" | "tt" | "kbd" | "samp" => style.code = true,
            "s" | "del" | "strike" => style.strike = true,
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
        self.quoted = outer_quoted;
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
    let mut walker = Walker { blocks: Vec::new(), runs: Vec::new(), kind: Kind::Line, depth: 0, marker: None, lists: Vec::new(), pre: false, continued: false, quoted: 0 };
    walker.element(cell, &style);
    walker.flush();
    let mut runs = Vec::new();
    for block in walker.blocks {
        if !runs.is_empty() {
            runs.push(Run::Text { text: " ".into(), bold: false, italic: false, code: false, strike: false });
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
                runs.push(Run::Text { text: rest[..start].to_owned(), bold: false, italic: false, code: false, strike: false });
            }
            runs.push(Run::Link { text: url.to_owned(), url: url.to_owned() });
            rest = &rest[start + url.len()..];
        }
        if !rest.is_empty() {
            runs.push(Run::Text { text: rest.to_owned(), bold: false, italic: false, code: false, strike: false });
        }
        let kind = if line.trim().is_empty() { Kind::Para } else if quoted > 0 { Kind::Quote } else { Kind::Line };
        blocks.push(Block { kind, depth: quoted, marker: None, continued: false, runs, rows: Vec::new() });
    }
    blocks
}

/// Where the earlier messages a reply carries below itself start, to fold them away: the quote at the
/// end of the message with the line that introduces it ("On Mon, Ada wrote:"), or a header block
/// ("From:" then "Sent:" or "Date:") as some clients write them unquoted. `None` when nothing is carried, or
/// when nothing would be left to read above it. A forward's header is kept: its text is the message.
pub fn quoted_tail(blocks: &[Block]) -> Option<usize> {
    let text = |b: &Block| b.runs.iter().map(|r| match r { Run::Text { text, .. } | Run::Link { text, .. } => text.as_str() }).collect::<String>().trim().to_owned();
    let empty = |b: &Block| b.kind != Kind::Rule && b.rows.is_empty() && text(b).is_empty();
    let starts = |b: &Block, words: &[&str]| {
        let t = text(b).to_lowercase();
        words.iter().any(|w| t.starts_with(w))
    };
    // The quote at the end, up to its introduction.
    let mut quote = None;
    for (i, b) in blocks.iter().enumerate().rev() {
        match b.kind {
            Kind::Quote => quote = Some(i),
            _ if empty(b) => {}
            _ => break,
        }
    }
    let quote = quote.map(|start| {
        let before = blocks[..start].iter().rposition(|b| !empty(b));
        match before {
            Some(i) if matches!(blocks[i].kind, Kind::Para | Kind::Line) && text(&blocks[i]).ends_with(':') && text(&blocks[i]).chars().count() <= 300 => i,
            _ => start,
        }
    });
    // A header block, and the rule or "Original Message" line above it.
    const FROM: [&str; 5] = ["from:", "da:", "de:", "von:", "van:"];
    const DATE: [&str; 8] = ["sent:", "date:", "inviato:", "data:", "gesendet:", "datum:", "envoyé", "enviado:"];
    let header = (1..blocks.len()).find(|&i| starts(&blocks[i], &FROM) && blocks[i + 1..].iter().filter(|b| !empty(b)).take(3).any(|b| starts(b, &DATE))).and_then(|i| {
        let above = blocks[..i].iter().rposition(|b| !empty(b))?;
        let line = text(&blocks[above]).to_lowercase();
        if ["forward", "inoltrat", "weitergeleitet", "transféré", "reenviado"].iter().any(|w| line.contains(w)) {
            return None;
        }
        Some(if blocks[above].kind == Kind::Rule || (line.starts_with("-----") && line.ends_with("-----")) { above } else { i })
    });
    let start = match (quote, header) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    blocks[..start].iter().any(|b| !empty(b) && b.kind != Kind::Rule).then_some(start)
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
    // A site's name, not any word with a dot: "aquí." ends a sentence, "7.0.4" is a version.
    let named = (text.starts_with("http://") || text.starts_with("https://") || (!text.contains(' ') && !text.contains('@'))) && crate::sender::claimed_domain(text).is_some();
    named && !url.to_ascii_lowercase().starts_with("mailto:") && !crate::sender::same_site(&host(text), &host(url))
}

/// Where a known redirect sends the reader, taken out of the link so it goes there directly; `None`
/// for any other link. Only services that carry the target in the link itself are known.
pub fn unwrap_redirect(url: &str) -> Option<String> {
    let parsed = blitz_traits::net::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = parsed.path();
    let param = |names: &[&str]| parsed.query_pairs().find(|(k, _)| names.contains(&k.as_ref())).map(|(_, v)| v.into_owned());
    let target = match host {
        "google.com" if path == "/url" => param(&["q", "url"]),
        "youtube.com" if path == "/redirect" => param(&["q"]),
        "l.facebook.com" | "lm.facebook.com" | "l.instagram.com" => param(&["u"]),
        "linkedin.com" if path.starts_with("/redir/") => param(&["url"]),
        "out.reddit.com" | "slack-redir.net" => param(&["url"]),
        "steamcommunity.com" if path.starts_with("/linkfilter") => param(&["url", "u"]),
        h if h.ends_with(".safelinks.protection.outlook.com") => param(&["url"]),
        // Proofpoint's third version keeps the address between "__" marks.
        "urldefense.com" if path.starts_with("/v3/__") => url.split_once("/v3/__").and_then(|(_, rest)| rest.split_once("__")).map(|(target, _)| target.to_owned()),
        _ => None,
    }?;
    let lower = target.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(target)
}

/// The link going straight where it leads (out of known redirects), without known tracking
/// parameters nor those the user named in `extra` (separated by spaces or commas; one ending in `*`
/// removes every parameter that starts so).
pub fn clean_url(url: &str, extra: &str) -> String {
    let mut url = url.to_owned();
    // A redirect may wrap another; a few unwrappings are plenty.
    for _ in 0..3 {
        match unwrap_redirect(&url) {
            Some(target) => url = target,
            None => break,
        }
    }
    strip_tracking(&url, extra)
}

fn strip_tracking(url: &str, extra: &str) -> String {
    const TRACKING: [&str; 25] = [
        "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid", "li_fat_id", "srsltid", "_gl", "mc_eid", "mc_cid", "mkt_tok", "igshid", "_hsenc", "_hsmi", "hsctatracking",
        "vero_id", "oly_enc_id", "oly_anon_id", "s_cid", "ref_src", "epik",
    ];
    let extra: Vec<String> = extra.split([',', ' ', '\n']).map(|w| w.trim().to_ascii_lowercase()).filter(|w| !w.is_empty()).collect();
    let named = |key: &str| extra.iter().any(|w| w.strip_suffix('*').map_or(w == key, |prefix| key.starts_with(prefix)));
    let (before_fragment, fragment) = url.split_once('#').map_or((url, None), |(a, b)| (a, Some(b)));
    let Some((base, query)) = before_fragment.split_once('?') else { return url.to_owned() };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
            !(key.starts_with("utm_") || TRACKING.contains(&key.as_str()) || named(&key))
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
    fn the_messages_a_reply_carries_are_folded_from_their_introduction() {
        let plain = plain_blocks("Sounds good.\n\nOn Mon, Ada wrote:\n> Hello there\n>\n>> older\n\n");
        assert_eq!(quoted_tail(&plain), Some(2));
        let html = blocks(r#"<p>Sounds good.</p><div class="gmail_quote"><div>On Mon, Ada &lt;ada@example.org&gt; wrote:<br></div><blockquote>Hello there</blockquote></div>"#);
        assert_eq!(texts(&html[quoted_tail(&html).unwrap()..])[0], "On Mon, Ada <ada@example.org> wrote:");
        let outlook = plain_blocks("Sounds good.\n\n-----Original Message-----\nFrom: Ada\nSent: Monday\nSubject: Plans\n\nHello there");
        assert_eq!(quoted_tail(&outlook), Some(2));
        let ruled = blocks("<p>Sounds good.</p><hr><p><b>From:</b> Ada<br><b>Sent:</b> Monday<br><b>Subject:</b> Plans</p><p>Hello there</p>");
        assert_eq!(ruled[quoted_tail(&ruled).unwrap()].kind, Kind::Rule);
    }

    #[test]
    fn answers_between_quotes_forwards_and_bare_quotes_stay_open() {
        assert_eq!(quoted_tail(&plain_blocks("> Can you come?\nYes.\n> At ten?\nFine.")), None);
        assert_eq!(quoted_tail(&plain_blocks("On Mon, Ada wrote:\n> Hello there")), None, "nothing above to read");
        assert_eq!(quoted_tail(&plain_blocks("See below.\n\n---------- Forwarded message ---------\nFrom: Ada\nDate: Monday\n\nHello")), None);
        assert_eq!(quoted_tail(&plain_blocks("Just text.\nFrom: the team")), None);
    }

    #[test]
    fn paragraphs_in_a_quote_are_quoted_at_their_depth() {
        // As Markdown writes a reply: a quote of paragraphs, one quote inside another.
        let html = beavermail_smtp::markdown_html("Thanks!\n\nOn Mon, Ada wrote:\n> Hello there\n>\n> > older quote\n\nBye");
        let shown: Vec<(Kind, u8)> = blocks(&html).iter().map(|b| (b.kind, b.depth)).collect();
        assert_eq!(shown, [(Kind::Para, 0), (Kind::Para, 0), (Kind::Quote, 1), (Kind::Quote, 2), (Kind::Para, 0)], "{html}");
    }

    #[test]
    fn a_broken_paragraph_has_its_room_after_its_last_line() {
        let kinds: Vec<Kind> = blocks("<p>one<br>two</p><ul><li>item</li></ul>").iter().map(|b| b.kind).collect();
        assert_eq!(kinds, [Kind::Line, Kind::Para, Kind::Item]);
    }

    #[test]
    fn a_line_break_in_a_list_item_continues_the_item() {
        let b = blocks("<ul><li>first<br>still first</li><li>second</li></ul>");
        let items: Vec<(bool, &str)> = b.iter().map(|b| (b.continued, match &b.runs[0] {
            Run::Text { text, .. } => text.as_str(),
            Run::Link { text, .. } => text.as_str(),
        })).collect();
        assert_eq!(items, [(false, "first"), (true, "still first"), (false, "second")]);
    }

    #[test]
    fn struck_text_keeps_its_line_through() {
        let b = blocks("<p>Price <del>10</del> <s>9</s> 8</p>");
        let struck: Vec<&str> = b[0].runs.iter().filter_map(|r| match r {
            Run::Text { text, strike: true, .. } => Some(text.trim()),
            _ => None,
        }).collect();
        assert_eq!(struck, ["10", "9"]);
    }

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
        assert_eq!(clean_url("https://x.example/p?id=7&utm_source=n&fbclid=abc#top", ""), "https://x.example/p?id=7#top");
        assert_eq!(clean_url("https://x.example/p?utm_medium=a&gclid=b", ""), "https://x.example/p");
        assert_eq!(clean_url("https://x.example/p?id=7&ref=mail&trk_x=1&trk_y=2", "ref, trk_*"), "https://x.example/p?id=7", "the user's own");
        assert_eq!(clean_url("https://x.example/p?reference=1", "ref"), "https://x.example/p?reference=1", "a name, not a start");
    }

    #[test]
    fn known_redirects_go_straight_to_their_target() {
        let wrapped = [
            "https://www.google.com/url?q=https%3A%2F%2Fshop.example%2Fa%3Fid%3D1%26utm_source%3Dx&sa=D",
            "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fshop.example%2Fa%3Fid%3D1&data=05%7C01&reserved=0",
            "https://l.facebook.com/l.php?u=https%3A%2F%2Fshop.example%2Fa%3Fid%3D1%26fbclid%3Dz&h=AT0",
            "https://urldefense.com/v3/__https://shop.example/a?id=1__;!!abc$",
        ];
        for url in wrapped {
            assert_eq!(clean_url(url, ""), "https://shop.example/a?id=1", "{url}");
        }
        assert_eq!(unwrap_redirect("https://www.google.com/search?q=https%3A%2F%2Fx.example"), None, "a search, not a redirect");
        assert_eq!(unwrap_redirect("https://www.google.com/url?q=javascript%3Aalert(1)"), None, "only web addresses");
        assert_eq!(unwrap_redirect("https://shop.example/out?url=https%3A%2F%2Fevil.example"), None, "unknown services are left as they are");
        assert_eq!(clean_url("https://x.example/p", ""), "https://x.example/p");
    }

    #[test]
    fn a_link_that_names_another_site_is_flagged() {
        assert!(looks_misleading("www.bank.com", "https://evil.example/login"));
        assert!(looks_misleading("https://bank.com/a", "https://evil.example/"));
        assert!(!looks_misleading("https://www.bank.com/a", "https://bank.com/b"));
        assert!(!looks_misleading("Click here", "https://evil.example/"));
        for text in ["aqui.", "7.0.4", "v2.1"] {
            assert!(!looks_misleading(text, "https://evil.example/"), "{text}");
        }
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
