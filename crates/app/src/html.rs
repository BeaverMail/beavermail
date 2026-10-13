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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: Kind,
    /// Nesting depth of lists and quotes.
    pub depth: u8,
    pub marker: Option<String>,
    pub runs: Vec<Run>,
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
            self.blocks.push(Block { kind: self.kind, depth: self.depth, marker, runs });
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
        blocks.push(Block { kind, depth: quoted, marker: None, runs });
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
    named && !url.to_ascii_lowercase().starts_with("mailto:") && host(text) != host(url)
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
    }

    #[test]
    fn huge_input_is_cut_instead_of_freezing() {
        let html = format!("<p>{}</p>", "x ".repeat(MAX_INPUT));
        assert!(blocks(&html).len() <= MAX_BLOCKS);
    }
}
