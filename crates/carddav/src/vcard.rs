// SPDX-License-Identifier: AGPL-3.0-only

//! vCards (RFC 6350, and 3.0 as most servers keep them): what BeaverMail shows of one, and changes
//! that touch only its name, notes and email addresses, every other line kept as it was.

/// What BeaverMail shows of a card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Card {
    pub uid: String,
    /// The formatted name (`FN`), or one made from `N`, or the organisation.
    pub name: String,
    /// Lower case.
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub organization: String,
    pub note: String,
}

/// One logical line: folded lines joined (RFC 6350 3.2).
pub(crate) fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => lines.push(raw.to_owned()),
        }
    }
    lines.retain(|l| !l.is_empty());
    lines
}

/// A property's name (upper case, without its group) and value, from a line `group.NAME;params:value`.
fn property(line: &str) -> Option<(String, &str)> {
    let (head, value) = split_unquoted(line)?;
    let name = head.split(';').next()?;
    let name = name.rsplit('.').next()?.to_ascii_uppercase();
    Some((name, value))
}

/// Splits at the first `:` that is not inside a quoted parameter value.
pub(crate) fn split_unquoted(line: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some((&line[..i], &line[i + 1..])),
            _ => {}
        }
    }
    None
}

pub(crate) fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (c, c == '\\') {
            (_, true) => match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            },
            (c, false) => out.push(c),
        }
    }
    out
}

pub(crate) fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace(',', "\\,").replace(';', "\\;").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// What a vCard says, for showing it.
pub fn parse(text: &str) -> Card {
    let mut card = Card::default();
    let mut structured = String::new();
    for line in unfold(text) {
        let Some((name, value)) = property(&line) else { continue };
        match name.as_str() {
            "UID" => card.uid = value.trim().to_owned(),
            "FN" => card.name = unescape(value).trim().to_owned(),
            // N is family;given;additional;prefix;suffix: shown "given family" when FN is missing.
            "N" => {
                let parts: Vec<String> = value.split(';').map(unescape).collect();
                structured = [parts.get(3), parts.get(1), parts.get(2), parts.first(), parts.get(4)].into_iter().flatten().filter(|p| !p.trim().is_empty()).cloned().collect::<Vec<_>>().join(" ");
            }
            "EMAIL" => {
                let email = unescape(value).trim().to_lowercase();
                if !email.is_empty() && !card.emails.contains(&email) {
                    card.emails.push(email);
                }
            }
            "TEL" => {
                let phone = unescape(value).trim().trim_start_matches("tel:").to_owned();
                if !phone.is_empty() {
                    card.phones.push(phone);
                }
            }
            "ORG" => card.organization = value.split(';').map(unescape).filter(|p| !p.trim().is_empty()).collect::<Vec<_>>().join(", "),
            "NOTE" => card.note = unescape(value),
            _ => {}
        }
    }
    if card.name.is_empty() {
        card.name = if structured.is_empty() { card.organization.clone() } else { structured };
    }
    card
}

/// How an address, a number or an email address is labelled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Label {
    Work,
    Home,
    /// For phones.
    Mobile,
    #[default]
    Other,
}

impl Label {
    /// From a line's parameters: `TYPE=WORK,INTERNET`, `TYPE=cell`, or 3.0's bare `TEL;HOME`.
    fn of(params: &str) -> Self {
        let params = params.to_ascii_lowercase();
        let has = |word: &str| params.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| t == word);
        match () {
            _ if has("cell") || has("mobile") || has("iphone") => Self::Mobile,
            _ if has("work") => Self::Work,
            _ if has("home") => Self::Home,
            _ => Self::Other,
        }
    }

    fn param(self, phone: bool) -> &'static str {
        match self {
            Self::Work => ";TYPE=work",
            Self::Home => ";TYPE=home",
            Self::Mobile if phone => ";TYPE=cell",
            _ => "",
        }
    }
}

/// A postal address (`ADR`), without the post office box and extended parts few use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Address {
    pub street: String,
    pub city: String,
    pub region: String,
    pub code: String,
    pub country: String,
}

/// Everything BeaverMail shows and edits of a card. Values are as written (not lower-cased), so an
/// unchanged one is recognised and its line kept as it was.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub given: String,
    pub family: String,
    pub organization: String,
    pub title: String,
    pub emails: Vec<(Label, String)>,
    pub phones: Vec<(Label, String)>,
    pub addresses: Vec<(Label, Address)>,
    pub urls: Vec<String>,
    /// As written, `1815-12-10` most often.
    pub birthday: String,
    pub note: String,
    /// The photo's bytes (JPEG or PNG), when the card holds it.
    // shortcut: a photo given by link (Google does so) is kept but not shown; fetch it to show it.
    pub photo: Option<Vec<u8>>,
}

/// One unfolded line, split.
struct Line {
    raw: String,
    group: Option<String>,
    /// Upper case.
    name: String,
    params: String,
    value: String,
}

fn split(raw: String) -> Option<Line> {
    let (head, value) = split_unquoted(&raw)?;
    let (name, params) = head.split_once(';').unwrap_or((head, ""));
    let (group, name) = match name.rsplit_once('.') {
        Some((group, name)) => (Some(group.to_ascii_lowercase()), name.to_ascii_uppercase()),
        None => (None, name.to_ascii_uppercase()),
    };
    let (params, value) = (params.to_owned(), value.to_owned());
    Some(Line { raw, group, name, params, value })
}

/// A value's `;`-separated parts, each unescaped.
fn parts(value: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n' | 'N') => out.last_mut().unwrap().push('\n'),
                Some(other) => out.last_mut().unwrap().push(other),
                None => {}
            },
            ';' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

/// What one line holds, for the lines BeaverMail edits.
#[derive(PartialEq)]
enum Item {
    Name,
    Email(Label, String),
    Phone(Label, String),
    Address(Label, Address),
    Url(String),
    Organization(String),
    Title(String),
    Birthday(String),
    Note(String),
    Photo(Option<Vec<u8>>),
}

fn item(line: &Line) -> Option<Item> {
    let text = || unescape(&line.value).trim().to_owned();
    Some(match line.name.as_str() {
        "FN" | "N" => Item::Name,
        "EMAIL" => Item::Email(Label::of(&line.params), text()),
        "TEL" => Item::Phone(Label::of(&line.params), text().trim_start_matches("tel:").to_owned()),
        "ADR" => {
            let p = parts(&line.value);
            let at = |i: usize| p.get(i).map(|s| s.trim().to_owned()).unwrap_or_default();
            Item::Address(Label::of(&line.params), Address { street: at(2), city: at(3), region: at(4), code: at(5), country: at(6) })
        }
        "URL" => Item::Url(line.value.trim().to_owned()),
        "ORG" => Item::Organization(parts(&line.value).into_iter().map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(", ")),
        "TITLE" => Item::Title(text()),
        "BDAY" => Item::Birthday(line.value.trim().to_owned()),
        "NOTE" => Item::Note(unescape(&line.value)),
        "PHOTO" => Item::Photo(photo(line)),
        _ => return None,
    })
}

/// The bytes of a photo held in the card: `data:image/jpeg;base64,…` (4.0) or `ENCODING=b` (3.0).
fn photo(line: &Line) -> Option<Vec<u8>> {
    use base64::Engine;
    let data = match line.value.trim().strip_prefix("data:") {
        Some(uri) => uri.split_once("base64,")?.1,
        None if line.params.to_ascii_lowercase().contains("encoding=b") => line.value.trim(),
        None => return None,
    };
    let data: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Everything BeaverMail edits of a card. Without a structured name, the formatted one is split
/// (the last word as the family name).
pub fn details(text: &str) -> Details {
    let mut out = Details::default();
    let mut formatted = String::new();
    for line in unfold(text).into_iter().filter_map(split) {
        match (line.name.as_str(), item(&line)) {
            ("FN", _) => formatted = unescape(&line.value).trim().to_owned(),
            ("N", _) => {
                let p = parts(&line.value);
                (out.family, out.given) = (p.first().map(|s| s.trim().to_owned()).unwrap_or_default(), p.get(1).map(|s| s.trim().to_owned()).unwrap_or_default());
            }
            (_, Some(Item::Email(label, v))) if !v.is_empty() => out.emails.push((label, v)),
            (_, Some(Item::Phone(label, v))) if !v.is_empty() => out.phones.push((label, v)),
            (_, Some(Item::Address(label, a))) => out.addresses.push((label, a)),
            (_, Some(Item::Url(v))) if !v.is_empty() => out.urls.push(v),
            (_, Some(Item::Organization(v))) => out.organization = v,
            (_, Some(Item::Title(v))) => out.title = v,
            (_, Some(Item::Birthday(v))) => out.birthday = v,
            (_, Some(Item::Note(v))) => out.note = v,
            (_, Some(Item::Photo(v))) => out.photo = v,
            _ => {}
        }
    }
    if out.given.is_empty() && out.family.is_empty() {
        let mut words: Vec<&str> = formatted.split_whitespace().collect();
        out.family = words.pop().unwrap_or_default().to_owned();
        out.given = words.join(" ");
    }
    out
}

/// The card with `new` details; every line BeaverMail does not edit (photo by link, anniversaries,
/// the server's own, labels of kept values) as it was, and so is each value left unchanged.
pub fn apply(text: &str, new: &Details) -> String {
    let old = details(text);
    if *new == old {
        return text.to_owned();
    }
    let lines: Vec<Line> = unfold(text).into_iter().filter_map(split).collect();
    let four = lines.iter().any(|l| l.name == "VERSION" && l.value.trim() == "4.0");
    let renamed = (&new.given, &new.family) != (&old.given, &old.family);
    // Values still wanted keep their lines, each once; what is left of them is written anew.
    let (mut emails, mut phones, mut addresses, mut urls) = (new.emails.clone(), new.phones.clone(), new.addresses.clone(), new.urls.clone());
    fn take<T: PartialEq>(list: &mut Vec<T>, wanted: T) -> bool {
        list.iter().position(|w| *w == wanted).map(|at| list.remove(at)).is_some()
    }
    let keep: Vec<bool> = lines
        .iter()
        .map(|line| match item(line) {
            None => true,
            Some(Item::Name) => !renamed,
            Some(Item::Email(l, v)) => take(&mut emails, (l, v)),
            Some(Item::Phone(l, v)) => take(&mut phones, (l, v)),
            Some(Item::Address(l, a)) => take(&mut addresses, (l, a)),
            Some(Item::Url(v)) => take(&mut urls, v),
            Some(Item::Organization(v)) => v == new.organization,
            Some(Item::Title(v)) => v == new.title,
            Some(Item::Birthday(v)) => v == new.birthday,
            Some(Item::Note(v)) => v.trim_end() == new.note.trim_end(),
            Some(Item::Photo(v)) => v == new.photo,
        })
        .collect();
    // A label written beside a value (Apple's `item1.X-ABLabel`) goes with it.
    let gone: Vec<String> = lines.iter().zip(&keep).filter(|(_, k)| !**k).filter_map(|(l, _)| l.group.clone()).collect();
    let mut added = Vec::new();
    if renamed {
        let name = format!("{} {}", new.given.trim(), new.family.trim()).trim().to_owned();
        added.push(format!("FN:{}", escape(if name.is_empty() { &new.organization } else { &name })));
        added.push(format!("N:{};{};;;", escape(new.family.trim()), escape(new.given.trim())));
    }
    added.extend(emails.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(l, v)| format!("EMAIL{}:{}", l.param(false), escape(v.trim()))));
    added.extend(phones.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(l, v)| format!("TEL{}:{}", l.param(true), escape(v.trim()))));
    for (l, a) in addresses.iter().filter(|(_, a)| *a != Address::default()) {
        let p: Vec<String> = [&a.street, &a.city, &a.region, &a.code, &a.country].iter().map(|v| escape(v.trim())).collect();
        added.push(format!("ADR{}:;;{}", l.param(false), p.join(";")));
    }
    added.extend(urls.iter().map(|u| u.split_whitespace().collect::<String>()).filter(|u| !u.is_empty()).map(|u| format!("URL:{u}")));
    let single = |old: &str, new: &str, name: &str, added: &mut Vec<String>| {
        if old != new && !new.trim().is_empty() {
            added.push(format!("{name}:{}", escape(new.trim())));
        }
    };
    single(&old.organization, &new.organization, "ORG", &mut added);
    single(&old.title, &new.title, "TITLE", &mut added);
    if old.birthday != new.birthday && !new.birthday.trim().is_empty() {
        added.push(format!("BDAY:{}", new.birthday.trim()));
    }
    single(old.note.trim_end(), new.note.trim_end(), "NOTE", &mut added);
    if old.photo != new.photo
        && let Some(bytes) = &new.photo
    {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let png = bytes.starts_with(b"\x89PNG");
        added.push(match (four, png) {
            (true, true) => format!("PHOTO:data:image/png;base64,{data}"),
            (true, false) => format!("PHOTO:data:image/jpeg;base64,{data}"),
            (false, true) => format!("PHOTO;ENCODING=b;TYPE=PNG:{data}"),
            (false, false) => format!("PHOTO;ENCODING=b;TYPE=JPEG:{data}"),
        });
    }
    let mut out = Vec::new();
    for (line, kept) in lines.into_iter().zip(keep) {
        if line.name == "END" && line.value.trim().eq_ignore_ascii_case("VCARD") {
            out.append(&mut added);
        }
        if kept && line.group.as_ref().is_none_or(|g| !gone.contains(g)) {
            out.push(line.raw);
        }
    }
    fold(&out)
}

/// "N:family;given;;;" from a name typed as "Given Family".
fn structured_name(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.split_last() {
        Some((family, given)) if !given.is_empty() => format!("N:{};{};;;", escape(family), escape(&given.join(" "))),
        Some((only, _)) => format!("N:{};;;;", escape(only)),
        None => "N:;;;;".to_owned(),
    }
}

/// A new card, in vCard 3.0, the version every server and client reads.
pub fn new_card(uid: &str, name: &str, emails: &[String], note: &str) -> String {
    let mut lines = vec!["BEGIN:VCARD".to_owned(), "VERSION:3.0".to_owned(), "PRODID:-//BeaverMail//EN".to_owned(), format!("UID:{uid}"), format!("FN:{}", escape(name.trim())), structured_name(name)];
    lines.extend(emails.iter().map(|e| format!("EMAIL;TYPE=INTERNET:{}", escape(e))));
    if !note.trim().is_empty() {
        lines.push(format!("NOTE:{}", escape(note.trim_end())));
    }
    lines.push("END:VCARD".to_owned());
    fold(&lines)
}

/// Lines joined with CRLF, each folded at 75 octets (on a character boundary).
fn fold(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        let mut rest = line.as_str();
        let mut first = true;
        while !rest.is_empty() {
            let limit = if first { 75 } else { 74 };
            let mut cut = rest.len().min(limit);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            if !first {
                out.push(' ');
            }
            out.push_str(&rest[..cut]);
            out.push_str("\r\n");
            rest = &rest[cut..];
            first = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE_CARD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:abc-1\r\nFN:Ada Lovelace\r\nN:Lovelace;Ada;;;\r\nitem1.EMAIL;TYPE=WORK:Ada@Example.org\r\nTEL;TYPE=CELL:+44 20 7946 0000\r\nBDAY:1815-12-10\r\nPHOTO;ENCODING=b;TYPE=JPEG:/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDA\r\n sLCxkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/\r\nNOTE:Met at the fair\\, 2026\r\nEND:VCARD\r\n";

    #[test]
    fn a_card_reads_with_its_name_addresses_phones_and_notes() {
        let card = parse(PHONE_CARD);
        assert_eq!((card.uid.as_str(), card.name.as_str(), card.note.as_str()), ("abc-1", "Ada Lovelace", "Met at the fair, 2026"));
        assert_eq!(card.emails, ["ada@example.org"]);
        assert_eq!(card.phones, ["+44 20 7946 0000"]);
        let phone_only = parse("BEGIN:VCARD\r\nVERSION:4.0\r\nN:Rossi;Mario;;Dr.;\r\nTEL;VALUE=uri:tel:+39-02-1234\r\nEND:VCARD\r\n");
        assert_eq!((phone_only.name.as_str(), phone_only.emails.len()), ("Dr. Mario Rossi", 0), "a name from N when FN is missing, and no email is fine");
        assert_eq!(phone_only.phones, ["+39-02-1234"]);
    }

    #[test]
    fn a_change_rewrites_only_what_changed() {
        let mut d = details(PHONE_CARD);
        assert_eq!((d.given.as_str(), d.family.as_str()), ("Ada", "Lovelace"));
        assert_eq!(d.emails, [(Label::Work, "Ada@Example.org".to_owned())]);
        assert_eq!(d.phones, [(Label::Mobile, "+44 20 7946 0000".to_owned())]);
        assert_eq!(apply(PHONE_CARD, &d), PHONE_CARD, "nothing changed: the card as it was");
        (d.given, d.family, d.note) = ("Augusta Ada".into(), "King".into(), "Countess".into());
        d.emails.push((Label::Home, "ada@home.example".into()));
        d.phones[0] = (Label::Work, "+44 20 7946 0001".into());
        d.addresses.push((Label::Home, Address { street: "12 St James's Square".into(), city: "London".into(), code: "SW1Y 4JH".into(), country: "UK".into(), ..Default::default() }));
        d.urls.push("https://ada.example".into());
        (d.organization, d.title, d.birthday) = ("Analytical Society, London".into(), "Countess".into(), "1815-12-10".into());
        let changed = apply(PHONE_CARD, &d);
        let back = details(&changed);
        assert_eq!(back, d, "{changed}");
        assert_eq!(parse(&changed).name, "Augusta Ada King");
        assert!(changed.contains("item1.EMAIL;TYPE=WORK:Ada@Example.org") && changed.contains("PHOTO;ENCODING=b"), "a kept value keeps its line:\n{changed}");
        assert!(changed.contains("TEL;TYPE=work:+44 20 7946 0001") && !changed.contains("7946 0000"));
        assert!(changed.lines().all(|l| l.len() <= 76), "folded");
    }

    #[test]
    fn a_removed_value_takes_its_label_and_a_photo_reads_back_in_either_version() {
        let apple = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bob\r\nN:;Bob;;;\r\nitem1.EMAIL:bob@x.org\r\nitem1.X-ABLabel:school\r\nEMAIL;TYPE=HOME:bob@y.org\r\nEND:VCARD\r\n";
        let mut d = details(apple);
        d.emails.remove(0);
        let changed = apply(apple, &d);
        assert!(!changed.contains("item1") && changed.contains("bob@y.org"), "{changed}");
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        for card in [apple, "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Bob\r\nEND:VCARD\r\n"] {
            let mut d = details(card);
            d.photo = Some(jpeg.clone());
            let with = apply(card, &d);
            assert_eq!(details(&with).photo.as_deref(), Some(jpeg.as_slice()), "{with}");
            d.photo = None;
            assert!(!apply(&with, &d).contains("PHOTO"));
        }
        assert!(apply("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Bob\r\nEND:VCARD\r\n", &Details { photo: Some(jpeg), ..details("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Bob\r\nEND:VCARD\r\n") }).contains("PHOTO:data:image/jpeg;base64,"));
    }

    #[test]
    fn a_new_card_reads_back() {
        let text = new_card("uid-9", "Bob Brown", &["bob@y.org".into()], "Line one\nline two; more");
        let card = parse(&text);
        assert_eq!((card.uid.as_str(), card.name.as_str(), card.note.as_str()), ("uid-9", "Bob Brown", "Line one\nline two; more"));
        assert_eq!(card.emails, ["bob@y.org"]);
        assert!(text.starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\n") && text.ends_with("END:VCARD\r\n"));
    }
}
