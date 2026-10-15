// SPDX-License-Identifier: AGPL-3.0-only

//! Account storage. Settings live in `accounts.toml` in the user's config directory;
//! passwords live only in the system credential store (Keychain, Credential Manager,
//! Secret Service), keyed by [`Account::key`]. A password is never written to the file.

use std::path::PathBuf;

use beavermail_core::{Account, Settings};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "beavermail";

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    account: Vec<Account>,
}

pub fn parse(text: &str) -> Result<Vec<Account>, String> {
    toml::from_str::<File>(text).map(|f| f.account).map_err(|e| e.to_string())
}

pub fn render(accounts: &[Account]) -> Result<String, String> {
    toml::to_string_pretty(&File { account: accounts.to_vec() }).map_err(|e| e.to_string())
}

fn path() -> Result<PathBuf, String> {
    config_file("accounts.toml")
}

fn config_file(name: &str) -> Result<PathBuf, String> {
    let dirs = ProjectDirs::from("", "", "beavermail").ok_or("no home directory")?;
    Ok(dirs.config_dir().join(name))
}

/// Writes `text` to `path` through a temp file, so a crash cannot leave it half-written.
fn write_atomic(path: &std::path::Path, text: &str) -> Result<(), String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    std::fs::create_dir_all(path.parent().ok_or("no config directory")?).map_err(io)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Preferences from `settings.toml`; the defaults if the file does not exist yet.
pub fn load_settings() -> Result<Settings, String> {
    let path = config_file("settings.toml")?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    write_atomic(&config_file("settings.toml")?, &toml::to_string_pretty(settings).map_err(|e| e.to_string())?)
}

/// Where the local mail archive lives: the user's data directory, not the config directory.
pub fn archive_path() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("mail.sqlite3"))
}

/// The directory holding the archive, the message files and the key file.
pub fn data_dir() -> Result<PathBuf, String> {
    let dirs = ProjectDirs::from("", "", "beavermail").ok_or("no home directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Accounts from the config file; none if the file does not exist yet.
pub fn load() -> Result<Vec<Account>, String> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Replaces the accounts file.
pub fn save(accounts: &[Account]) -> Result<(), String> {
    write_atomic(&path()?, &render(accounts)?)
}

fn entry(account: &Account) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, &account.key()).map_err(|e| format!("credential store: {e}"))
}

pub fn password(account: &Account) -> Result<String, String> {
    entry(account)?.get_password().map_err(|e| format!("credential store: {e}"))
}

pub fn set_password(account: &Account, password: &str) -> Result<(), String> {
    entry(account)?.set_password(password).map_err(|e| format!("credential store: {e}"))
}

/// Removes the stored password; a missing one is not an error.
pub fn forget_password(account: &Account) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("credential store: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beavermail_core::{Server, Tls};

    fn account(user: &str) -> Account {
        let imap = Server { host: "mail.example.org".into(), port: 143, tls: Tls::StartTls };
        let smtp = Server { host: "mail.example.org".into(), port: 465, tls: Tls::Tls };
        Account { name: "Work".into(), user: user.into(), email: user.into(), imap, smtp, save_sent: true, keep: beavermail_core::Keep::default(), signature: String::new() }
    }

    #[test]
    fn file_round_trips() {
        let accounts = vec![account("a@example.org"), account("b@example.org")];
        assert_eq!(parse(&render(&accounts).unwrap()).unwrap(), accounts);
    }

    #[test]
    fn files_without_an_email_still_load() {
        let text = "[[account]]\nname = \"Old\"\nuser = \"a@example.org\"\n[account.imap]\nhost = \"h\"\nport = 143\ntls = \"starttls\"\n[account.smtp]\nhost = \"h\"\nport = 587\ntls = \"starttls\"\n";
        let accounts = parse(text).unwrap();
        assert_eq!(accounts[0].email, "");
        assert_eq!(accounts[0].address(), "a@example.org");
    }

    #[test]
    fn settings_round_trip_and_tolerate_missing_keys() {
        let mut settings = Settings { display: beavermail_core::DisplayMode::Simplified, ..Settings::default() };
        settings.set_sender_mode("a@example.org", beavermail_core::DisplayMode::Plain);
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }

    #[test]
    fn empty_file_means_no_accounts() {
        assert_eq!(parse("").unwrap(), vec![]);
    }

    #[test]
    fn rendered_file_has_no_secret_field() {
        assert!(!render(&[account("a@example.org")]).unwrap().contains("password"));
    }
}
