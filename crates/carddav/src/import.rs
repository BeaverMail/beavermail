// SPDX-License-Identifier: AGPL-3.0-only

//! Contacts read from a file another app exported: a vCard file (one card or many, any version) or
//! a CSV file from Google Contacts, Outlook or Thunderbird. Each comes out as a vCard 3.0 to keep.

use crate::vcard::{self, Card};

/// The cards of a vCard file, each as its text, ready to keep under `uid()`: one of version 3.0 or
/// 4.0 as it was (every field kept, the UID replaced), an older one (2.1, as Outlook writes them)
/// remade from what BeaverMail reads of it.
pub fn from_vcf(text: &str, mut uid: impl FnMut() -> String) -> Vec<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut cards = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for line in unfold_qp(text) {
        let upper = line.to_ascii_uppercase();
        if upper == "BEGIN:VCARD" {
            lines.clear();
        }
        lines.push(line);
        if upper == "END:VCARD" {
            let version = lines.iter().find_map(|l| l.to_ascii_uppercase().strip_prefix("VERSION:").map(|v| v.trim().to_owned())).unwrap_or_default();
            let card = if version == "3.0" || version == "4.0" {
                let mut kept: Vec<String> = lines.iter().filter(|l| !l.to_ascii_uppercase().starts_with("UID:") && !l.to_ascii_uppercase().starts_with("UID;")).cloned().collect();
                kept.insert(1, format!("UID:{}", uid()));
                vcard::fold(&kept)
            } else {
                let read = vcard::parse(&lines.join("\r\n"));
                vcard::card_text(&uid(), &read)
            };
            cards.push(card);
            lines.clear();
        }
    }
    cards
}

/// Logical lines of a vCard file: folded lines joined, and quoted-printable values (vCard 2.1)
/// decoded, with their soft line breaks.
fn unfold_qp(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut raw = vcard::unfold(text).into_iter();
    while let Some(mut line) = raw.next() {
        let Some((head, _)) = vcard::split_unquoted(&line) else {
            out.push(line);
            continue;
        };
        if !head.to_ascii_uppercase().contains("QUOTED-PRINTABLE") {
            out.push(line);
            continue;
        }
        // A value ending in "=" goes on on the next line.
        while line.ends_with('=') {
            line.pop();
            match raw.next() {
                Some(next) => line.push_str(next.trim_start()),
                None => break,
            }
        }
        let (head, value) = vcard::split_unquoted(&line).expect("checked above");
        let head: Vec<&str> = head.split(';').filter(|p| !p.to_ascii_uppercase().contains("QUOTED-PRINTABLE") && !p.to_ascii_uppercase().starts_with("CHARSET=")).collect();
        out.push(format!("{}:{}", head.join(";"), decode_qp(value)));
    }
    out
}

fn decode_qp(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], value.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'=', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The contacts of a CSV file, by its first row's column names as Google Contacts, Outlook and
/// Thunderbird write them; an error when no column holds a name or an email address.
pub fn from_csv(text: &str) -> Result<Vec<Card>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = csv_rows(text).into_iter();
    let header: Vec<String> = rows.next().unwrap_or_default().into_iter().map(|h| h.trim().to_lowercase()).collect();
    let find = |names: &[&str]| header.iter().position(|h| names.contains(&h.as_str()));
    let full = find(&["name", "display name", "full name"]);
    let (first, middle, last) = (find(&["first name", "given name"]), find(&["middle name", "additional name"]), find(&["last name", "family name", "surname"]));
    let organization = find(&["company", "organization", "organization 1 - name", "organisation"]);
    let note = find(&["notes", "note"]);
    let is = |h: &str, words: &[&str]| words.iter().any(|w| h.contains(w)) && !h.contains("type") && !h.contains("label") && !h.contains("display");
    let emails: Vec<usize> = (0..header.len()).filter(|&i| is(&header[i], &["e-mail", "email"])).collect();
    let phones: Vec<usize> = (0..header.len()).filter(|&i| is(&header[i], &["phone", "mobile", "fax", "pager"])).collect();
    if emails.is_empty() && full.is_none() && first.is_none() && last.is_none() {
        return Err("no column of names or email addresses: export the contacts as vCard, or as Google or Outlook CSV".into());
    }
    let mut cards = Vec::new();
    for row in rows {
        let cell = |i: Option<usize>| i.and_then(|i| row.get(i)).map_or("", |c| c.trim());
        // Google keeps several values in one cell, separated by " ::: ".
        let many = |columns: &[usize]| -> Vec<String> { columns.iter().filter_map(|&i| row.get(i)).flat_map(|c| c.split(":::")).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).collect() };
        let mut card = Card { emails: many(&emails).into_iter().map(|e| e.to_lowercase()).filter(|e| e.contains('@') && !e.contains(' ')).collect(), phones: many(&phones), organization: cell(organization).to_owned(), note: cell(note).to_owned(), ..Card::default() };
        card.emails.dedup();
        card.name = match cell(full) {
            "" => [cell(first), cell(middle), cell(last)].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" "),
            name => name.to_owned(),
        };
        if !card.name.is_empty() || !card.emails.is_empty() || !card.phones.is_empty() {
            cards.push(card);
        }
    }
    Ok(cards)
}

/// The rows of a CSV file (RFC 4180): fields split at commas, or at semicolons when the first line
/// has more of them (as a spreadsheet in many languages saves it); a quoted field may hold either,
/// a doubled quote and line breaks.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let first = text.lines().next().unwrap_or_default();
    let separator = if first.matches(';').count() > first.matches(',').count() { ';' } else { ',' };
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let (mut quoted, mut chars) = (false, text.chars().peekable());
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == separator && !quoted => row.push(std::mem::take(&mut field)),
            '\r' if !quoted => {}
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r: &Vec<String>| r.iter().any(|f| !f.trim().is_empty()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("u{n}")
        }
    }

    #[test]
    fn a_vcf_file_gives_each_card_with_a_new_uid() {
        let file = "\u{feff}BEGIN:VCARD\r\nVERSION:3.0\r\nUID:old\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.org\r\nADR:;;1 Main St;;;;\r\nEND:VCARD\r\nBEGIN:VCARD\r\nVERSION:4.0\r\nFN:Bob\r\nEND:VCARD\r\n";
        let cards = from_vcf(file, uids());
        assert_eq!(cards.len(), 2);
        let ada = vcard::parse(&cards[0]);
        assert_eq!((ada.uid.as_str(), ada.name.as_str(), ada.emails.as_slice()), ("u1", "Ada Lovelace", ["ada@example.org".to_owned()].as_slice()));
        assert!(cards[0].contains("ADR:;;1 Main St;;;;"), "every field kept");
        assert_eq!(vcard::parse(&cards[1]).uid, "u2");
    }

    #[test]
    fn an_outlook_vcard_21_is_decoded_and_remade() {
        let file = "BEGIN:VCARD\r\nVERSION:2.1\r\nN;CHARSET=utf-8;ENCODING=QUOTED-PRINTABLE:N=C3=A9ri;Ren=C3=A9\r\nFN;CHARSET=utf-8;ENCODING=QUOTED-PRINTABLE:Ren=C3=A9 N=C3=A9=\r\nri\r\nEMAIL;PREF;INTERNET:rene@example.org\r\nTEL;CELL:+1 555 0100\r\nEND:VCARD\r\n";
        let cards = from_vcf(file, uids());
        assert!(cards[0].contains("VERSION:3.0"));
        let rene = vcard::parse(&cards[0]);
        assert_eq!((rene.name.as_str(), rene.emails[0].as_str(), rene.phones[0].as_str()), ("René Néri", "rene@example.org", "+1 555 0100"));
    }

    #[test]
    fn google_outlook_and_thunderbird_csv_columns_are_read() {
        let google = "Name,Given Name,Family Name,E-mail 1 - Type,E-mail 1 - Value,Phone 1 - Type,Phone 1 - Value,Notes\nAda Lovelace,Ada,Lovelace,* Home,ada@example.org ::: ada@example.com,Mobile,+1 555 0100,\"Met at the \"\"club\"\", twice\"\n";
        let ada = &from_csv(google).unwrap()[0];
        assert_eq!(ada.name, "Ada Lovelace");
        assert_eq!(ada.emails, ["ada@example.org", "ada@example.com"]);
        assert_eq!((ada.phones[0].as_str(), ada.note.as_str()), ("+1 555 0100", "Met at the \"club\", twice"));
        let outlook = "First Name,Middle Name,Last Name,Company,E-mail Address,E-mail Display Name,Mobile Phone\r\nBob,,Smith,Example Inc,BOB@example.org,Bob Smith (bob@example.org),555 0101\r\n";
        let bob = &from_csv(outlook).unwrap()[0];
        assert_eq!((bob.name.as_str(), bob.organization.as_str(), bob.emails.as_slice(), bob.phones.as_slice()), ("Bob Smith", "Example Inc", ["bob@example.org".to_owned()].as_slice(), ["555 0101".to_owned()].as_slice()));
        let thunderbird = "First Name;Last Name;Display Name;Primary Email;Secondary Email\nCarol;;Carol;carol@example.org;\n";
        assert_eq!(from_csv(thunderbird).unwrap()[0].emails, ["carol@example.org"]);
        assert!(from_csv("Colour,Size\nred,9\n").is_err(), "no names or addresses: not contacts");
    }
}
