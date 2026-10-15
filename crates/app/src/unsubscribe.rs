// SPDX-License-Identifier: AGPL-3.0-only

//! Leaving a mailing list from its `List-Unsubscribe` header: which way to use, and what a `mailto:`
//! link asks to be sent.

use blitz_traits::net::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// A POST to this `https` link (RFC 8058); nothing opens in the browser.
    OneClick(String),
    /// An email to send: (to, subject, text).
    Mail(String, String, String),
    /// The sender's unsubscribe page, to open in the browser.
    Page(String),
}

impl Method {
    /// The host the user's request goes to, to name it before asking.
    pub fn host(&self) -> String {
        match self {
            Self::OneClick(link) | Self::Page(link) => Url::parse(link).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default(),
            Self::Mail(to, ..) => to.rsplit('@').next().unwrap_or_default().to_owned(),
        }
    }
}

/// The way to leave the list: a one-click POST when the sender offers it and `trusted` (the account's
/// server verified the sender), else an email, else the page. An email reveals no more than a reply,
/// while a page may track the visit, so the page comes last.
pub fn method(links: &[String], one_click: bool, trusted: bool) -> Option<Method> {
    let https = links.iter().find(|l| l.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("https:")));
    if one_click && trusted && let Some(link) = https {
        return Some(Method::OneClick(link.clone()));
    }
    links.iter().find_map(|l| mail(l)).or_else(|| https.map(|l| Method::Page(l.clone())))
}

/// A `mailto:` link as the email it asks for; "unsubscribe" when it names no subject.
fn mail(link: &str) -> Option<Method> {
    let url = Url::parse(link).ok().filter(|u| u.scheme() == "mailto")?;
    let to = percent_decode(url.path());
    if !to.contains('@') || to.contains([',', ' ']) {
        return None; // one address only: this is not a way to send mail to a list of strangers
    }
    let field = |name: &str| url.query_pairs().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.into_owned());
    Some(Method::Mail(to, field("subject").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "unsubscribe".into()), field("body").unwrap_or_default()))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn one_click_only_for_a_verified_sender_that_offers_it() {
        let both = links(&["mailto:leave@shop.example", "https://shop.example/u"]);
        assert_eq!(method(&both, true, true), Some(Method::OneClick("https://shop.example/u".into())));
        assert_eq!(method(&both, true, false), Some(Method::Mail("leave@shop.example".into(), "unsubscribe".into(), String::new())), "unverified: an email instead");
        assert_eq!(method(&both, false, true), Some(Method::Mail("leave@shop.example".into(), "unsubscribe".into(), String::new())));
        assert_eq!(method(&links(&["https://shop.example/u"]), false, true), Some(Method::Page("https://shop.example/u".into())));
        assert_eq!(method(&[], true, true), None);
    }

    #[test]
    fn a_mailto_link_gives_its_address_subject_and_text() {
        let m = method(&links(&["mailto:list%2Bleave@lists.example?Subject=Remove%20me&body=id%3D42"]), false, false);
        assert_eq!(m, Some(Method::Mail("list+leave@lists.example".into(), "Remove me".into(), "id=42".into())));
        assert_eq!(m.unwrap().host(), "lists.example");
        assert_eq!(method(&links(&["mailto:a@x.example,b@y.example"]), false, false), None, "one address only");
    }
}
