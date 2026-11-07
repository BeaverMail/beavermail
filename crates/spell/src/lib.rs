// SPDX-License-Identifier: AGPL-3.0-only

//! Spell checking of the messages being written, on Hunspell dictionaries: the ones the system has,
//! or ones downloaded into the data directory from LibreOffice's collection.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// A dictionary ready to check words.
pub struct Speller(spellbook::Dictionary);

/// Most suggestions offered for one word.
const SUGGESTIONS: usize = 5;

impl Speller {
    /// Reads a dictionary from its two files, in the charset its `.aff` names.
    pub fn load(aff: &[u8], dic: &[u8]) -> Result<Self, String> {
        let encoding = charset(aff);
        let (aff, _, _) = encoding.decode(aff);
        let (dic, _, _) = encoding.decode(dic);
        spellbook::Dictionary::new(&aff, &dic).map(Self).map_err(|e| e.to_string())
    }

    /// Reads the dictionary at `aff` and the `.dic` beside it.
    pub fn open(aff: &Path) -> Result<Self, String> {
        let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
        Self::load(&read(aff)?, &read(&aff.with_extension("dic"))?)
    }

    /// Whether `word` is spelled right, or `known` to the user.
    pub fn check(&self, word: &str, known: &impl Fn(&str) -> bool) -> bool {
        let word = word.replace('’', "'");
        known(&word) || known(&word.to_lowercase()) || self.0.check(&word)
    }

    /// The words of `text` spelled wrong, as byte ranges, leaving out what is not prose (see [`words`]).
    pub fn misspelled(&self, text: &str, known: impl Fn(&str) -> bool) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        for mut range in words(text) {
            if self.check(&text[range.clone()], &known) {
                continue;
            }
            // "po'" keeps its apostrophe, a closing quote does not.
            if let Some(word) = text[range.clone()].strip_suffix(['\'', '’']) {
                if self.check(word, &known) {
                    continue;
                }
                range.end = range.start + word.len();
            }
            out.push(range);
        }
        out
    }

    /// The best few spellings for `word`, best first.
    pub fn suggest(&self, word: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.0.suggest(&word.replace('’', "'"), &mut out);
        out.truncate(SUGGESTIONS);
        out
    }
}

/// The charset on the `SET` line of an `.aff`; UTF-8 when there is none or it is unknown.
fn charset(aff: &[u8]) -> &'static encoding_rs::Encoding {
    aff.split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"SET "))
        .and_then(|name| encoding_rs::Encoding::for_label(name.trim_ascii()))
        .unwrap_or(encoding_rs::UTF_8)
}

/// The words of `text` worth checking, as byte ranges. Left out: quoted lines (`>`), code (between
/// backticks or in ``` blocks), the signature (after `-- `), links and addresses, and words with
/// digits, in capitals (acronyms) or with a capital inside (names like "BeaverMail").
pub fn words(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let at = start;
        start += line.len();
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if bare == "-- " || bare == "--" {
            break;
        }
        if fenced || bare.trim_start().starts_with('>') {
            continue;
        }
        let mut code = false;
        let mut pos = 0;
        for chunk in bare.split_inclusive(char::is_whitespace) {
            let chunk_at = at + pos;
            pos += chunk.len();
            let skip = chunk.contains("://") || chunk.contains('@') || chunk.starts_with("www.");
            let mut i = 0;
            while i < chunk.len() {
                let c = chunk[i..].chars().next().unwrap_or(' ');
                if c == '`' {
                    code = !code;
                }
                if code || skip || !c.is_alphanumeric() {
                    i += c.len_utf8();
                    continue;
                }
                let end = word_end(chunk, i);
                if prose(&chunk[i..end]) {
                    out.push(chunk_at + i..chunk_at + end);
                }
                i = end;
            }
        }
    }
    out
}

/// Where the word starting at `from` ends: letters and digits, with apostrophes and hyphens between
/// them, and an apostrophe after them ("po'").
fn word_end(s: &str, from: usize) -> usize {
    let mut end = from;
    let mut chars = s[from..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let joins = matches!(c, '\'' | '’' | '-');
        let next_letter = chars.peek().is_some_and(|(_, n)| n.is_alphanumeric());
        if c.is_alphanumeric() || (joins && next_letter) || (c != '-' && joins) {
            end = from + i + c.len_utf8();
        } else {
            break;
        }
        if joins && !next_letter {
            break;
        }
    }
    end
}

/// A word, not a number, an acronym or a name written with a capital inside.
fn prose(word: &str) -> bool {
    let letters = || word.chars().filter(|c| c.is_alphabetic());
    !word.chars().any(|c| c.is_numeric())
        && !(letters().count() > 1 && letters().all(char::is_uppercase))
        && !letters().skip(1).any(char::is_uppercase)
}

/// A dictionary that can be downloaded: its code, its name, and its path in LibreOffice's collection.
pub struct Language {
    pub code: &'static str,
    pub name: &'static str,
    path: &'static str,
}

/// Dictionaries offered for download. Left out: Danish (its flags fail to parse) and Turkish (over 20 MB).
pub const LANGUAGES: [Language; 31] = [
    Language { code: "bg_BG", name: "Bulgarian", path: "bg_BG/bg_BG" },
    Language { code: "ca", name: "Catalan", path: "ca/dictionaries/ca" },
    Language { code: "hr_HR", name: "Croatian", path: "hr_HR/hr_HR" },
    Language { code: "cs_CZ", name: "Czech", path: "cs_CZ/cs_CZ" },
    Language { code: "nl_NL", name: "Dutch", path: "nl_NL/nl_NL" },
    Language { code: "en_AU", name: "English (Australia)", path: "en/en_AU" },
    Language { code: "en_CA", name: "English (Canada)", path: "en/en_CA" },
    Language { code: "en_GB", name: "English (UK)", path: "en/en_GB" },
    Language { code: "en_US", name: "English (US)", path: "en/en_US" },
    Language { code: "et_EE", name: "Estonian", path: "et_EE/et_EE" },
    Language { code: "fr_FR", name: "French", path: "fr_FR/dictionaries/fr" },
    Language { code: "de_AT", name: "German (Austria)", path: "de/de_AT_frami" },
    Language { code: "de_DE", name: "German (Germany)", path: "de/de_DE_frami" },
    Language { code: "de_CH", name: "German (Switzerland)", path: "de/de_CH_frami" },
    Language { code: "el_GR", name: "Greek", path: "el_GR/el_GR" },
    Language { code: "hu_HU", name: "Hungarian", path: "hu_HU/hu_HU" },
    Language { code: "it_IT", name: "Italian", path: "it_IT/it_IT" },
    Language { code: "lv_LV", name: "Latvian", path: "lv_LV/lv_LV" },
    Language { code: "lt_LT", name: "Lithuanian", path: "lt_LT/lt" },
    Language { code: "nb_NO", name: "Norwegian (Bokmål)", path: "no/nb_NO" },
    Language { code: "pl_PL", name: "Polish", path: "pl_PL/pl_PL" },
    Language { code: "pt_BR", name: "Portuguese (Brazil)", path: "pt_BR/pt_BR" },
    Language { code: "pt_PT", name: "Portuguese (Portugal)", path: "pt_PT/pt_PT" },
    Language { code: "ro_RO", name: "Romanian", path: "ro/ro_RO" },
    Language { code: "ru_RU", name: "Russian", path: "ru_RU/ru_RU" },
    Language { code: "sk_SK", name: "Slovak", path: "sk_SK/sk_SK" },
    Language { code: "sl_SI", name: "Slovenian", path: "sl_SI/sl_SI" },
    Language { code: "es_ES", name: "Spanish (Spain)", path: "es/es_ES" },
    Language { code: "es_MX", name: "Spanish (Mexico)", path: "es/es_MX" },
    Language { code: "sv_SE", name: "Swedish", path: "sv_SE/dictionaries/sv_SE" },
    Language { code: "uk_UA", name: "Ukrainian", path: "uk_UA/uk_UA" },
];

/// The name of the dictionary `code`, or the code itself when it is not one of [`LANGUAGES`].
pub fn name(code: &str) -> &str {
    LANGUAGES.iter().find(|l| l.code == code).map_or(code, |l| l.name)
}

/// Where the system keeps its Hunspell dictionaries.
const SYSTEM_DIRS: [&str; 3] = ["/usr/share/hunspell", "/usr/share/myspell", "/usr/share/myspell/dicts"];

/// The dictionaries found, by code (the name of the file), each with the path of its `.aff`: those in
/// `own` (downloaded) first, then the system's.
pub fn installed(own: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in std::iter::once(own.to_path_buf()).chain(SYSTEM_DIRS.iter().map(PathBuf::from)) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut found: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "aff") && p.with_extension("dic").is_file())
            .collect();
        found.sort();
        for path in found {
            let code = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            if !out.iter().any(|(c, _)| *c == code) {
                out.push((code, path));
            }
        }
    }
    out
}

/// The collection's version, fixed so a dictionary downloaded today is the one tested.
const SOURCE: &str = "https://raw.githubusercontent.com/LibreOffice/dictionaries/c0865a85eb67dc377f2cf8e8fe2c4fa9e53f0dcb";
const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Downloads dictionary `code` into `dir`, as `code.aff` and `code.dic`; a file is in place only once
/// complete, the `.aff` last, so a dictionary half downloaded is never found.
pub fn download(code: &str, dir: &Path) -> Result<PathBuf, String> {
    let language = LANGUAGES.iter().find(|l| l.code == code).ok_or_else(|| format!("no dictionary for {code}"))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(60))).build().into();
    for ext in ["dic", "aff"] {
        let url = format!("{SOURCE}/{}.{ext}", language.path);
        let bytes = agent
            .get(&url)
            .call()
            .and_then(|mut r| r.body_mut().with_config().limit(MAX_BYTES).read_to_vec())
            .map_err(|e| format!("{url}: {e}"))?;
        let path = dir.join(format!("{code}.{ext}"));
        let part = path.with_extension(format!("{ext}.part"));
        std::fs::write(&part, bytes).and_then(|()| std::fs::rename(&part, &path)).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(dir.join(format!("{code}.aff")))
}

/// Removes the downloaded dictionary `code` from `dir`.
pub fn remove(code: &str, dir: &Path) -> Result<(), String> {
    for ext in ["aff", "dic"] {
        let path = dir.join(format!("{code}.{ext}"));
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{}: {e}", path.display())),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny dictionary written for the tests: "house", "the" and "po'".
    fn speller() -> Speller {
        Speller::load(b"SET UTF-8\nTRY esianrtolcdugmphbyfvkwz'\n", b"3\nhouse\nthe\npo'\n").unwrap()
    }

    fn checked(text: &str) -> Vec<&str> {
        speller().misspelled(text, |w| w == "alice").into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn finds_words_spelled_wrong() {
        assert_eq!(checked("The hous, the house."), ["hous"]);
        assert_eq!(checked("Teh house\nhouze"), ["Teh", "houze"]);
    }

    #[test]
    fn leaves_out_what_is_not_prose() {
        assert_eq!(checked("> quoted lien\nhouse"), Vec::<&str>::new(), "quoted line");
        assert_eq!(checked("see `foo bar` and\n```\nbaz qux\n```\n"), ["see", "and"], "code");
        assert_eq!(checked("visit https://example.com/xyz or bob@example.org www.example.org"), ["visit", "or"]);
        assert_eq!(checked("IMAP v2 BeaverMail 3rd"), Vec::<&str>::new(), "acronyms, digits, inner capitals");
        assert_eq!(checked("house\n-- \nAlice Smoth"), Vec::<&str>::new(), "signature");
    }

    #[test]
    fn apostrophes_and_hyphens() {
        assert_eq!(checked("po' house"), Vec::<&str>::new(), "an apostrophe ending the word");
        assert_eq!(checked("'house' 'hous'"), ["hous"], "quotes are not part of the word");
        assert_eq!(checked("the-house the-hous"), ["the-hous"], "one word across the hyphen, each side checked");
        assert_eq!(checked("house - the"), Vec::<&str>::new(), "a dash alone");
        assert_eq!(checked("po’"), Vec::<&str>::new(), "the typographic apostrophe");
    }

    #[test]
    fn the_users_words_and_case() {
        assert_eq!(checked("alice Alice"), Vec::<&str>::new());
        assert_eq!(checked("House HOUSE"), Vec::<&str>::new());
    }

    #[test]
    fn suggests_spellings() {
        assert_eq!(speller().suggest("hous").first().map(String::as_str), Some("house"));
    }

    #[test]
    fn reads_other_charsets() {
        let aff = b"SET ISO8859-1\n";
        let speller = Speller::load(aff, b"1\nm\xe4dchen\n").unwrap();
        assert!(speller.check("mädchen", &|_| false));
    }

    #[test]
    fn finds_and_removes_downloaded_ones() {
        let dir = std::env::temp_dir().join(format!("beavermail-spell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("xx_XX.aff"), "SET UTF-8\n").unwrap();
        std::fs::write(dir.join("xx_XX.dic"), "1\nhouse\n").unwrap();
        std::fs::write(dir.join("yy_YY.aff"), "SET UTF-8\n").unwrap();
        assert!(installed(&dir).iter().any(|(c, p)| c == "xx_XX" && *p == dir.join("xx_XX.aff")));
        assert!(!installed(&dir).iter().any(|(c, _)| c == "yy_YY"), "no .dic beside it");
        assert!(Speller::open(&dir.join("xx_XX.aff")).unwrap().check("house", &|_| false));
        remove("xx_XX", &dir).unwrap();
        assert!(!installed(&dir).iter().any(|(c, _)| c == "xx_XX"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
