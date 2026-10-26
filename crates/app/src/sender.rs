// SPDX-License-Identifier: AGPL-3.0-only

//! Telling the person who really sent a message from the name they chose to show.
//!
//! The display name is free text: "Your Bank" <evil@attacker.io> is a valid sender. The list shows the
//! domain next to the name, and a name that claims another address or site, or a domain written with
//! look-alike characters, is flagged.

use beavermail_core::AuthResult;

/// What the account's own server says about who sent a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Authenticity {
    /// The server checked and the domain in the From address is really the sender's.
    Verified,
    /// The server checked and the message does not come from the domain it claims.
    Failed,
    /// The server said nothing we can rely on.
    Unknown,
}

/// The verdict of the account's own server on a message from `address`. Only headers written by that
/// server count: a sender can put any `Authentication-Results` it likes into its message, but those sit
/// below the one the receiving server adds and name another host.
pub fn authenticity(results: &[AuthResult], server_host: &str, address: &str) -> Authenticity {
    let from = domain(address);
    let ours: Vec<_> = results.iter().filter(|r| same_server(&r.server, server_host)).flat_map(|r| &r.checks).collect();
    // Only a failure that is about the domain in the From address counts, whichever check noticed it.
    let about_from = |d: &str| d.is_empty() || same_site(d, &from);
    let passed = |method: &str| ours.iter().any(|c| c.method == method && c.result == "pass" && !c.domain.is_empty() && same_site(&c.domain, &from));
    if ours.iter().any(|c| c.method == "dmarc" && matches!(c.result.as_str(), "fail" | "permerror") && about_from(&c.domain)) {
        return Authenticity::Failed;
    }
    if passed("dmarc") || passed("dkim") || passed("spf") {
        return Authenticity::Verified;
    }
    if ours.iter().any(|c| matches!(c.method.as_str(), "dkim" | "spf") && c.result == "fail" && about_from(&c.domain)) {
        return Authenticity::Failed;
    }
    Authenticity::Unknown
}

/// The account's own server's checks of a message from `address`, as the reader says them: one line
/// ("SPF pass  ·  DKIM pass  ·  DMARC pass" when they do not verify the sender) and every check, with
/// the domain it was about, for its hover text. `None` when that server wrote none.
pub fn checks_line(results: &[AuthResult], server_host: &str, address: &str) -> Option<(String, String)> {
    let from = domain(address);
    let ours: Vec<(&str, _)> = results.iter().filter(|r| same_server(&r.server, server_host)).flat_map(|r| r.checks.iter().map(move |c| (r.server.as_str(), c))).filter(|(_, c)| matches!(c.method.as_str(), "spf" | "dkim" | "dmarc")).collect();
    let server = ours.first()?.0;
    let line = if authenticity(results, server_host, address) == Authenticity::Verified {
        format!("Sender domain verified by your server: {from}")
    } else {
        // Per method, the check about the From domain if there is one (a message can carry several signatures).
        let shown: Vec<String> = ["spf", "dkim", "dmarc"]
            .iter()
            .filter_map(|m| {
                let of: Vec<_> = ours.iter().map(|(_, c)| c).filter(|c| c.method == *m).collect();
                let check = of.iter().find(|c| same_site(&c.domain, &from)).or(of.first())?;
                Some(format!("{} {}", m.to_uppercase(), check.result))
            })
            .collect();
        format!("Not verified by your server: {}", shown.join("  ·  "))
    };
    let mut tip: Vec<String> = ours
        .iter()
        .map(|(_, c)| match (c.method.as_str(), c.domain.is_empty()) {
            (_, true) => format!("{}: {}", c.method.to_uppercase(), c.result),
            ("dkim", false) => format!("DKIM: {}, signed by {}", c.result, c.domain),
            (m, false) => format!("{}: {}, for {}", m.to_uppercase(), c.result, c.domain),
        })
        .collect();
    tip.push(format!("Checked by {server}"));
    Some((line, tip.join("\n")))
}

/// The authserv-id names the same mail system as the account's server: the same host, or two hosts
/// of one domain.
fn same_server(authserv: &str, host: &str) -> bool {
    let (a, h) = (authserv.to_lowercase(), host.to_lowercase());
    !a.is_empty() && h.contains('.') && (a == h || registrable(&a) == registrable(&h))
}

/// The domain an organisation registered: "mail.shop.example.co.uk" is "example.co.uk".
/// shortcut: a two-letter country code after a short generic label (co, com, org…) is taken as a public
/// suffix; the Public Suffix List knows the exceptions ("ltd.uk", "github.io"). It also decides who the
/// spam checks take for one organisation, so use the list once a dependency for it is acceptable.
pub fn registrable(domain: &str) -> String {
    let labels: Vec<&str> = domain.trim_end_matches('.').split('.').collect();
    let n = labels.len();
    let country_suffix = n >= 3 && labels[n - 1].len() == 2 && ["co", "com", "net", "org", "gov", "edu", "ac", "gob", "or", "ne"].contains(&labels[n - 2]);
    let keep = if country_suffix { 3 } else { 2 };
    labels[n.saturating_sub(keep)..].join(".").to_lowercase()
}

/// A provider where anyone can open a mailbox: its domain says nothing about who is writing.
pub fn public_mail(domain: &str) -> bool {
    const PROVIDERS: [&str; 50] = [
        "gmail", "googlemail", "hotmail", "outlook", "live", "msn", "yahoo", "ymail", "rocketmail", "aol", "icloud", "me", "mac", "proton", "protonmail", "pm", "gmx",
        "web", "mail", "email", "yandex", "zoho", "tutanota", "tuta", "fastmail", "hey", "libero", "virgilio", "alice", "tim", "tin", "tiscali", "inwind", "iol", "fastwebnet",
        "orange", "wanadoo", "free", "laposte", "sfr", "t-online", "telefonica", "qq", "163", "126", "naver", "rambler", "seznam", "wp", "interia",
    ];
    let name = registrable(domain);
    PROVIDERS.contains(&name.split('.').next().unwrap_or(""))
}

/// A domain as people read it: punycode ("xn--…") decoded.
pub fn readable(domain: &str) -> String {
    idna::domain_to_unicode(domain).0
}

/// What a reader takes a domain for: look-alike characters folded into one (the Unicode UTS #39
/// skeleton), so "paypa1.com", "pаypal.com" with a Cyrillic "а" and "xn--pypal-4ve.com" all read as
/// "paypal.com", and "rn" reads as "m".
pub fn skeleton(domain: &str) -> String {
    unicode_security::skeleton(&readable(domain).to_lowercase()).collect::<String>().to_lowercase()
}

/// A domain written to pass for a Latin one: a label that mixes scripts, or whose every character
/// reads as a Latin letter. A domain in its own script ("münchen.de", "日本.jp") is not.
pub fn disguised(domain: &str) -> bool {
    use unicode_security::MixedScript;
    readable(domain).split('.').any(|label| !label.is_ascii() && (!label.is_single_script() || unicode_security::skeleton(label).all(|c| c.is_ascii())))
}

/// The part of the address after the last `@`, lower case; empty when there is none.
pub fn domain(address: &str) -> String {
    address.rsplit_once('@').map_or(String::new(), |(_, d)| d.trim().trim_end_matches('>').to_lowercase())
}

/// Why this sender deserves a second look, if it does.
pub fn warning(name: &str, address: &str) -> Option<&'static str> {
    let real = domain(address);
    if real.is_empty() {
        return None;
    }
    if disguised(&real) {
        return Some("The sender's domain is written with look-alike characters");
    }
    if names_the_address(name, address) {
        return None;
    }
    let claimed = name.split(|c: char| c.is_whitespace() || "<>()[]\",;:".contains(c)).filter_map(claimed_domain).find(|d| !same_site(d, &real));
    claimed.map(|_| "The name shows an address or site that is not the one this was sent from")
}

/// A name that is only the address, or its part before the `@` ("ada.lovelace" from
/// ada.lovelace@example.org): it claims nothing the address does not show.
pub fn names_the_address(name: &str, address: &str) -> bool {
    let (name, address) = (name.trim().trim_matches('"').to_lowercase(), address.trim().to_lowercase());
    !name.is_empty() && (name == address || address.rsplit_once('@').is_some_and(|(local, _)| name == local))
}

/// The domain a word of a display name claims to be: "support@bank.com" or "www.bank.com".
pub fn claimed_domain(word: &str) -> Option<String> {
    let word = word.trim_matches(|c: char| c == '.' || c == '/' || c == '\'' || c == '-');
    let host = word.rsplit_once('@').map_or(word, |(_, d)| d);
    let host = host.strip_prefix("http://").or_else(|| host.strip_prefix("https://")).unwrap_or(host);
    let host = host.split('/').next().unwrap_or("").to_lowercase();
    let (_, tld) = host.rsplit_once('.')?;
    (tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()) && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')).then_some(host)
}

/// The same site, or one a subdomain of the other ("mail.bank.com" and "bank.com").
pub fn same_site(a: &str, b: &str) -> bool {
    let (a, b) = (a.strip_prefix("www.").unwrap_or(a), b.strip_prefix("www.").unwrap_or(b));
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn organisations_are_told_apart_from_public_mailboxes() {
        assert_eq!(registrable("e-invoice.partsco.com"), "partsco.com");
        assert_eq!(registrable("mail.shop.example.co.uk"), "example.co.uk");
        assert_eq!(registrable("correo.gob.es"), "correo.gob.es");
        assert_eq!(registrable("domainco.com"), "domainco.com");
        for d in ["gmail.com", "hotmail.it", "yahoo.co.uk", "libero.it", "mail.ru", "orange.es", "icloud.com"] {
            assert!(public_mail(d), "{d}");
        }
        for d in ["domainco.com", "mail.domainco.com", "partsco.com", "ownsite.example", "unicredit.eu"] {
            assert!(!public_mail(d), "{d}");
        }
    }

    #[test]
    fn the_domain_is_what_follows_the_at() {
        assert_eq!(domain("Support@Attacker.IO"), "attacker.io");
        assert_eq!(domain("no address"), "");
    }

    #[test]
    fn ordinary_senders_are_not_flagged() {
        for (name, address) in [("Ada Lovelace", "ada@example.org"), ("Mr. Smith", "smith@corp.com"), ("Bank", "no-reply@bank.com"), ("Bank mail", "x@mail.bank.com"), ("bank.com", "x@www.bank.com"), ("J.R.R. Tolkien", "j@x.org"), ("", "a@b.org"), ("ada.lovelace", "ada.lovelace@school.example.org")] {
            assert_eq!(warning(name, address), None, "{name} <{address}>");
        }
    }

    #[test]
    fn a_name_claiming_another_address_or_site_is_flagged() {
        assert!(warning("support@paypal.com", "evil@attacker.io").is_some());
        assert!(warning("PayPal <service@paypal.com>", "evil@attacker.io").is_some());
        assert!(warning("www.mybank.com security", "x@attacker.io").is_some());
        assert!(warning("https://mybank.com/help", "x@attacker.io").is_some());
        assert!(warning("Nome Banca", "evil@attacker.io").is_none(), "a plain name cannot be told from a real one: the domain next to it is what shows it");
    }

    #[test]
    fn look_alike_domains_are_flagged() {
        assert!(warning("Bank", "x@xn--pypal-4ve.com").is_some());
        assert!(warning("Bank", "x@pаypal.com").is_some(), "Cyrillic а");
        assert!(warning("Apple", "x@аррӏе.com").is_some(), "every letter Cyrillic");
        assert!(warning("Stadt", "x@münchen.de").is_none(), "a name in its own script");
        assert!(warning("Stadt", "x@xn--mnchen-3ya.de").is_none());
        assert!(warning("日本", "x@日本.jp").is_none());
    }

    fn checks(server: &str, list: &[(&str, &str, &str)]) -> Vec<AuthResult> {
        vec![AuthResult { server: server.into(), checks: list.iter().map(|(m, r, d)| beavermail_core::AuthCheck { method: (*m).into(), result: (*r).into(), domain: (*d).into() }).collect() }]
    }

    #[test]
    fn only_the_account_servers_own_checks_count() {
        let host = "mail.example.org";
        let good = checks("mail.example.org", &[("dmarc", "pass", "bank.com"), ("dkim", "pass", "bank.com")]);
        assert_eq!(authenticity(&good, host, "x@bank.com"), Authenticity::Verified);
        // Written by the sender: the same claim from another host is worth nothing.
        let forged = checks("mx.attacker.io", &[("dmarc", "pass", "bank.com")]);
        assert_eq!(authenticity(&forged, host, "x@bank.com"), Authenticity::Unknown);
        let failed = checks("example.org", &[("dmarc", "fail", "bank.com"), ("spf", "pass", "attacker.io")]);
        assert_eq!(authenticity(&failed, host, "x@bank.com"), Authenticity::Failed);
        assert_eq!(authenticity(&[], host, "x@bank.com"), Authenticity::Unknown);
    }

    #[test]
    fn the_servers_checks_are_said_in_one_line_and_each_on_hover() {
        let host = "mail.example.org";
        let good = checks(host, &[("spf", "pass", "bank.com"), ("dkim", "pass", "bank.com"), ("dmarc", "pass", "bank.com")]);
        let (line, tip) = checks_line(&good, host, "x@bank.com").unwrap();
        assert_eq!(line, "Sender domain verified by your server: bank.com");
        assert_eq!(tip, "SPF: pass, for bank.com\nDKIM: pass, signed by bank.com\nDMARC: pass, for bank.com\nChecked by mail.example.org");
        let poor = checks(host, &[("spf", "softfail", "bank.com"), ("dkim", "pass", "mailer.io"), ("dkim", "none", "bank.com"), ("arc", "pass", "")]);
        assert_eq!(checks_line(&poor, host, "x@bank.com").unwrap().0, "Not verified by your server: SPF softfail  ·  DKIM none");
        assert_eq!(checks_line(&checks("mx.attacker.io", &[("dmarc", "pass", "bank.com")]), host, "x@bank.com"), None, "written by the sender");
        assert_eq!(checks_line(&[], host, "x@bank.com"), None);
    }

    #[test]
    fn a_pass_for_another_domain_does_not_verify_this_sender() {
        let host = "mail.example.org";
        let other = checks(host, &[("dkim", "pass", "attacker.io"), ("spf", "pass", "attacker.io")]);
        assert_eq!(authenticity(&other, host, "x@bank.com"), Authenticity::Unknown);
        let alone = checks(host, &[("spf", "fail", "bank.com")]);
        assert_eq!(authenticity(&alone, host, "x@bank.com"), Authenticity::Failed);
        let subdomain = checks(host, &[("dkim", "pass", "bank.com")]);
        assert_eq!(authenticity(&subdomain, host, "news@mail.bank.com"), Authenticity::Verified);
    }
}
