// SPDX-License-Identifier: AGPL-3.0-only

//! The dictionaries of the spell check: those on this computer, the one loaded, and getting another.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use beavermail_spell::{LANGUAGES, Speller};

pub struct Dictionaries {
    /// Where downloaded dictionaries are kept.
    pub dir: PathBuf,
    /// The dictionaries on this computer, by code, with their `.aff`.
    pub installed: Vec<(String, PathBuf)>,
    /// Their codes, the languages a message can be checked in.
    pub codes: Vec<String>,
    /// The dictionary in use, by code.
    loaded: Option<(String, Arc<Speller>)>,
    /// The dictionary being read or downloaded.
    pub busy: Option<String>,
    /// The last dictionary that could not be had, and why; not asked for again until chosen again.
    pub failed: Option<(String, String)>,
}

impl Dictionaries {
    pub fn new(dir: PathBuf) -> Self {
        let mut out = Self { dir, installed: Vec::new(), codes: Vec::new(), loaded: None, busy: None, failed: None };
        out.rescan();
        out
    }

    /// Looks again for the dictionaries on this computer.
    pub fn rescan(&mut self) {
        self.installed = beavermail_spell::installed(&self.dir);
        self.codes = self.installed.iter().map(|(c, _)| c.clone()).collect();
    }

    /// The dictionary `code` once loaded.
    pub fn speller(&self, code: &str) -> Option<&Speller> {
        self.loaded.as_ref().filter(|(c, _)| c == code).map(|(_, s)| s.as_ref())
    }

    /// Whether `code` was downloaded here (and can be removed), not the system's.
    pub fn downloaded(&self, code: &str) -> bool {
        self.installed.iter().any(|(c, p)| c == code && p.parent() == Some(self.dir.as_path()))
    }

    /// The work that gets dictionary `code`: read from this computer, or downloaded first when
    /// `download` (the user asked: fetching it tells a server). `None` when it is loaded or on its way,
    /// cannot be had without a download, or failed and `retry` is not set.
    pub fn want(&mut self, code: &str, download: bool, retry: bool) -> Option<impl FnOnce() -> Result<Speller, String> + Send + 'static> {
        let failed = self.failed.as_ref().is_some_and(|(c, _)| c == code);
        let here = self.codes.iter().any(|c| c == code);
        if self.speller(code).is_some() || self.busy.as_deref() == Some(code) || (failed && !retry) || code.is_empty() || (!here && !download) {
            return None;
        }
        self.busy = Some(code.to_owned());
        self.failed = None;
        let (code, dir) = (code.to_owned(), self.dir.clone());
        let path = self.installed.iter().find(|(c, _)| *c == code).map(|(_, p)| p.clone());
        Some(move || Speller::open(&match path {
            Some(path) => path,
            None => beavermail_spell::download(&code, &dir)?,
        }))
    }

    /// A dictionary asked for with [`Dictionaries::want`] arrived, or could not be had.
    pub fn arrived(&mut self, code: String, result: Result<Arc<Speller>, String>) {
        if self.busy.as_ref() == Some(&code) {
            self.busy = None;
        }
        match result {
            Ok(speller) => {
                self.loaded = Some((code, speller));
                self.rescan();
            }
            Err(e) => self.failed = Some((code, e)),
        }
    }

    /// Removes the downloaded dictionary `code`.
    pub fn remove(&mut self, code: &str) -> Result<(), String> {
        let out = beavermail_spell::remove(code, &self.dir);
        if self.loaded.as_ref().is_some_and(|(c, _)| c == code) {
            self.loaded = None;
        }
        self.rescan();
        out
    }

    /// The language to check in when none was chosen: the system's, when there is a dictionary for it.
    /// shortcut: the system's language comes from LANG, which Windows and macOS do not set; there it is
    /// US English until one is chosen in Settings.
    pub fn default_code(&self) -> String {
        let lang = ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|v| std::env::var(v).ok().filter(|v| !v.is_empty())).unwrap_or_default();
        best(&lang, &self.codes).unwrap_or_else(|| "en_US".to_owned())
    }
}

/// The dictionary for locale `lang` ("it_IT.UTF-8"): the same code, or one of its language
/// ("it_CH" finds "it_IT"); those on this computer first.
fn best(lang: &str, installed: &[String]) -> Option<String> {
    let code = lang.split(['.', '@']).next().unwrap_or("");
    let language = code.split(['_', '-']).next().unwrap_or("");
    if language.is_empty() || language == "C" || language == "POSIX" {
        return None;
    }
    let all = || installed.iter().map(String::as_str).chain(LANGUAGES.iter().map(|l| l.code));
    let of_language = |c: &str| c == language || c.starts_with(&format!("{language}_"));
    all().find(|c| *c == code).or_else(|| all().find(|c| of_language(c))).map(str::to_owned)
}

/// The dictionaries directory inside the data directory.
pub fn dir(data: &Path) -> PathBuf {
    data.join("dictionaries")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_systems_language() {
        let installed = ["es_ES".to_owned(), "it_CH".to_owned()];
        assert_eq!(best("it_IT.UTF-8", &installed).as_deref(), Some("it_IT"), "the same code, to download");
        assert_eq!(best("it_SM", &installed).as_deref(), Some("it_CH"), "one of its language here");
        assert_eq!(best("es_AR.UTF-8", &installed).as_deref(), Some("es_ES"));
        assert_eq!(best("ca_ES@valencia", &installed).as_deref(), Some("ca"));
        assert_eq!(best("C.UTF-8", &installed), None);
        assert_eq!(best("", &installed), None);
    }
}
