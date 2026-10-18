// SPDX-License-Identifier: AGPL-3.0-only

//! Address books on a CardDAV server (RFC 6352): finding them, reading their cards, and changing
//! them without overwriting a change made meanwhile elsewhere (each card's ETag is checked).

pub mod sync;
pub mod vcard;

pub use sync::{Synced, fields, sync};

use base64::Engine;
use beavermail_core::MailError;

/// A card on the server: where it is and which version of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub href: String,
    pub etag: String,
}

/// What became of a change sent to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    /// Done; the card's new version, when the server says it.
    Done(Option<String>),
    /// The card changed on the server since it was read: nothing was written.
    Conflict,
}

/// An address book a server has, as found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub url: String,
    pub name: String,
    /// Cards can be written there: false for those the server makes itself (such as one of people
    /// recently written to, or the directory of a server's users).
    pub writable: bool,
}

/// One address book, and how to sign in to it.
#[derive(Clone, Debug)]
pub struct Book {
    /// The address book's own URL (a collection), ending in `/`.
    pub url: String,
    user: String,
    password: String,
}

const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:current-user-principal/><c:addressbook-home-set/><d:resourcetype/><d:displayname/><d:current-user-privilege-set/></d:prop></d:propfind>"#;
const PROPFIND_ETAGS: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:resourcetype/></d:prop></d:propfind>"#;

impl Book {
    /// An address book already found, at its URL.
    pub fn open(url: &str, user: &str, password: &str) -> Self {
        Self { url: url.to_owned(), user: user.to_owned(), password: password.to_owned() }
    }

    /// The first of the user's address books, from a server address or a domain (see [`Self::discover_all`]).
    pub fn discover(start: &str, user: &str, password: &str) -> Result<Self, MailError> {
        let found = Self::discover_all(start, user, password)?.into_iter().next().ok_or_else(|| MailError::Backend("the account has no address book".into()))?;
        Ok(Self::open(&found.url, user, password))
    }

    /// Every address book of the user, from a server address or a domain
    /// (`https://dav.example.org/`, `example.org`): the address book itself, or the account's home,
    /// found through `/.well-known/carddav` (RFC 6764) when needed.
    pub fn discover_all(start: &str, user: &str, password: &str) -> Result<Vec<Found>, MailError> {
        let start = start.trim();
        let start = if start.contains("://") { start.to_owned() } else { format!("https://{start}") };
        let probe = Self::open(&start, user, password);
        let mut tried = Vec::new();
        for candidate in [start.clone(), format!("{}/.well-known/carddav", origin(&start))] {
            match probe.find_from(&candidate) {
                Ok(found) => return Ok(found),
                Err(MailError::Auth(e)) => return Err(MailError::Auth(e)),
                Err(e) => tried.push(e.to_string()),
            }
        }
        Err(MailError::Backend(format!("no address book found at {start}: {}", tried.join("; "))))
    }

    /// The address books reached from `url`: itself if it is one, else those of its principal's home.
    fn find_from(&self, url: &str) -> Result<Vec<Found>, MailError> {
        let (at, answer) = self.dav("PROPFIND", url, "0", PROPFIND_HOME)?;
        let found = responses(&answer)?;
        let first = found.first().ok_or_else(|| MailError::Backend("the server said nothing".into()))?;
        if first.addressbook {
            return Ok(vec![Found { url: absolute(&at, &first.href), name: first.name.clone().unwrap_or_default(), writable: first.writable }]);
        }
        let home = match &first.home {
            Some(home) => absolute(&at, home),
            None => {
                let principal = first.principal.as_ref().ok_or_else(|| MailError::Backend("not a CardDAV server".into()))?;
                let (at, answer) = self.dav("PROPFIND", &absolute(&at, principal), "0", PROPFIND_HOME)?;
                let home = responses(&answer)?.into_iter().find_map(|r| r.home).ok_or_else(|| MailError::Backend("the account has no address book home".into()))?;
                absolute(&at, &home)
            }
        };
        let (at, answer) = self.dav("PROPFIND", &home, "1", PROPFIND_HOME)?;
        let books: Vec<Found> = responses(&answer)?.into_iter().filter(|r| r.addressbook).map(|r| Found { url: absolute(&at, &r.href), name: r.name.unwrap_or_default(), writable: r.writable }).collect();
        if books.is_empty() { Err(MailError::Backend("the account has no address book".into())) } else { Ok(books) }
    }

    /// The cards of the address book, with their versions.
    pub fn list(&self) -> Result<Vec<Remote>, MailError> {
        let (_, answer) = self.dav("PROPFIND", &self.url, "1", PROPFIND_ETAGS)?;
        let own = path(&self.url);
        Ok(responses(&answer)?.into_iter().filter(|r| !r.collection && path(&r.href) != own).filter_map(|r| Some(Remote { etag: r.etag?, href: r.href })).collect())
    }

    /// The cards at `hrefs`, as (where and which version, vCard text), asked for in batches.
    pub fn fetch(&self, hrefs: &[String]) -> Result<Vec<(Remote, String)>, MailError> {
        let mut out = Vec::new();
        for batch in hrefs.chunks(100) {
            let wanted: String = batch.iter().map(|h| format!("<d:href>{}</d:href>", xml_escape(h))).collect();
            let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><c:addressbook-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><c:address-data/></d:prop>{wanted}</c:addressbook-multiget>"#);
            let (_, answer) = self.dav("REPORT", &self.url, "1", &body)?;
            out.extend(responses(&answer)?.into_iter().filter_map(|r| Some((Remote { etag: r.etag?, href: r.href }, r.data?))));
        }
        Ok(out)
    }

    /// Where a new card with this UID goes.
    pub fn new_href(&self, uid: &str) -> String {
        new_href(&self.url, uid)
    }

    /// Writes a card: a new one when `etag` is `None` (refused if one is there already), else only if
    /// the server still has that version.
    pub fn put(&self, href: &str, vcard: &str, etag: Option<&str>) -> Result<Written, MailError> {
        let condition = match etag {
            Some(etag) => ("If-Match", etag.to_owned()),
            None => ("If-None-Match", "*".to_owned()),
        };
        let (status, headers, _) = self.send("PUT", &absolute(&self.url, href), &[("Content-Type", "text/vcard; charset=utf-8"), (condition.0, &condition.1)], vcard)?;
        match status {
            200..=299 => Ok(Written::Done(headers)),
            412 => Ok(Written::Conflict),
            _ => Err(refused(status, "write a card")),
        }
    }

    /// Deletes a card if the server still has that version; one already gone counts as deleted.
    pub fn delete(&self, href: &str, etag: &str) -> Result<Written, MailError> {
        let (status, _, _) = self.send("DELETE", &absolute(&self.url, href), &[("If-Match", etag)], "")?;
        match status {
            200..=299 | 404 => Ok(Written::Done(None)),
            412 => Ok(Written::Conflict),
            _ => Err(refused(status, "delete a card")),
        }
    }

    /// A WebDAV request with an XML body; its multistatus answer and the URL that gave it (after redirects).
    fn dav(&self, method: &str, url: &str, depth: &str, body: &str) -> Result<(String, String), MailError> {
        let mut url = url.to_owned();
        // Followed here, keeping the sign-in: the well-known address usually redirects.
        for _ in 0..5 {
            let (status, location, answer) = self.send_raw(method, &url, &[("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")], body)?;
            match status {
                207 => return Ok((url, answer)),
                301 | 302 | 307 | 308 => url = absolute(&url, &location.ok_or_else(|| MailError::Backend("a redirect without a place".into()))?),
                _ => return Err(refused(status, "read the address book")),
            }
        }
        Err(MailError::Backend("too many redirects".into()))
    }

    /// A request; its status, the ETag it answers with, and its text.
    fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> Result<(u16, Option<String>, String), MailError> {
        let (status, _, text, etag) = self.request(method, url, headers, body)?;
        Ok((status, etag, text))
    }

    /// A request; its status, the Location it answers with, and its text.
    fn send_raw(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> Result<(u16, Option<String>, String), MailError> {
        let (status, location, text, _) = self.request(method, url, headers, body)?;
        Ok((status, location, text))
    }

    fn request(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> Result<(u16, Option<String>, String, Option<String>), MailError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .max_redirects(0)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into();
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.user, self.password));
        let method = ureq::http::Method::from_bytes(method.as_bytes()).map_err(|e| MailError::Backend(e.to_string()))?;
        let mut request = ureq::http::Request::builder().method(method).uri(url).header("Authorization", format!("Basic {auth}")).header("User-Agent", "BeaverMail");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(body.to_owned()).map_err(|e| MailError::Backend(e.to_string()))?;
        let mut response = agent.run(request).map_err(|e| MailError::Offline(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(MailError::Auth("the address book server refused the user or password".into()));
        }
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let (location, etag) = (header("Location"), header("ETag"));
        let text = response.body_mut().read_to_string().map_err(|e| MailError::Offline(e.to_string()))?;
        Ok((status, location, text, etag))
    }
}

/// Where a new card with this UID goes in the address book at `book_url`.
pub fn new_href(book_url: &str, uid: &str) -> String {
    let safe: String = uid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    format!("{}{safe}.vcf", path(book_url))
}

/// A UID for a card made here: unique by the time and a counter.
pub fn new_uid() -> String {
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("beavermail-{nanos:x}-{:x}", COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn refused(status: u16, what: &str) -> MailError {
    MailError::Backend(format!("the address book server would not {what} (HTTP {status})"))
}

/// What one `<response>` of a multistatus says.
#[derive(Default)]
struct Response {
    href: String,
    etag: Option<String>,
    data: Option<String>,
    addressbook: bool,
    collection: bool,
    principal: Option<String>,
    home: Option<String>,
    name: Option<String>,
    /// From the privileges the server grants (RFC 3744); writable when it does not say.
    writable: bool,
}

fn responses(xml: &str) -> Result<Vec<Response>, MailError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| MailError::Backend(format!("unreadable answer from the address book server: {e}")))?;
    let is = |n: &roxmltree::Node<'_, '_>, name: &str| n.is_element() && n.tag_name().name() == name;
    let text_of = |n: roxmltree::Node<'_, '_>, name: &str| n.descendants().find(|d| is(d, name)).and_then(|d| d.text()).map(|t| t.trim().to_owned());
    Ok(doc
        .descendants()
        .filter(|n| is(n, "response"))
        .map(|r| {
            let mut out = Response { href: r.children().find(|c| is(c, "href")).and_then(|h| h.text()).unwrap_or_default().trim().to_owned(), writable: true, ..Default::default() };
            // A property counts only from a propstat whose status is 200 (a 404 lists what the server lacks).
            let props = r.children().filter(|c| is(c, "propstat")).filter(|p| text_of(*p, "status").is_none_or(|s| s.contains(" 200"))).filter_map(|p| p.children().find(|c| is(c, "prop")));
            for prop in props {
                for p in prop.children().filter(|c| c.is_element()) {
                    match p.tag_name().name() {
                        "getetag" => out.etag = p.text().map(|t| t.trim().to_owned()),
                        "address-data" => out.data = p.text().map(str::to_owned),
                        "resourcetype" => {
                            out.collection = p.children().any(|c| is(&c, "collection"));
                            out.addressbook = p.children().any(|c| is(&c, "addressbook"));
                        }
                        "current-user-principal" => out.principal = text_of(p, "href"),
                        "addressbook-home-set" => out.home = text_of(p, "href"),
                        "current-user-privilege-set" => out.writable = p.descendants().any(|d| ["all", "write", "write-content", "bind"].iter().any(|w| is(&d, w))),
                        "displayname" => out.name = p.text().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
                        _ => {}
                    }
                }
            }
            out
        })
        .collect())
}

/// `https://host:port` of a URL.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    format!("{scheme}://{}", rest.split('/').next().unwrap_or_default())
}

/// The path of a URL (or the href itself when it is one).
fn path(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/".to_owned(), |i| rest[i..].to_owned()),
        None => url.to_owned(),
    }
}

/// An href from an answer, made absolute against the URL that gave it.
fn absolute(base: &str, href: &str) -> String {
    if href.contains("://") {
        href.to_owned()
    } else if href.starts_with('/') {
        format!("{}{href}", origin(base))
    } else {
        format!("{}/{href}", base.trim_end_matches('/'))
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
