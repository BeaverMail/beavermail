//! The diagnostic log: timings and errors written to `beavermail-diagnostics.log` in the app's data
//! folder while the user has it on, for them to send when something is slow or fails. It never holds
//! what mail says: no text, subjects, addresses or names, only message numbers, times and errors.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Past this size the log starts again, the earlier one kept as `beavermail-diagnostics.old.log`.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

struct Logger {
    file: Mutex<Option<File>>,
}

static LOGGER: Logger = Logger { file: Mutex::new(None) };

/// The log's path in the data folder `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join("beavermail-diagnostics.log")
}

/// Starts writing to the log in `dir` (`Some`), or stops (`None`).
pub fn set(dir: Option<&Path>) {
    let _ = log::set_logger(&LOGGER); // once; later calls only change the file and the level
    let file = dir.and_then(|dir| {
        let path = path(dir);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
            let _ = std::fs::rename(&path, dir.join("beavermail-diagnostics.old.log"));
        }
        std::fs::create_dir_all(dir).ok()?;
        OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let on = file.is_some();
    *LOGGER.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = file;
    log::set_max_level(if on { log::LevelFilter::Debug } else { log::LevelFilter::Off });
    if on {
        log::info!("BeaverMail {} on {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    }
}

impl log::Log for Logger {
    /// Everything of this app's own crates; of the libraries under it, only their warnings and errors.
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.target().starts_with("beavermail") || meta.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Some(file) = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
            let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(file, "{time} {:<5} {}: {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {
        if let Some(file) = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_log_is_written_only_while_on() {
        let dir = std::env::temp_dir().join(format!("beavermail-diag-{}", std::process::id()));
        super::set(Some(&dir));
        log::debug!(target: "beavermail", "message 7: read in 2ms");
        log::debug!(target: "some_library", "not wanted");
        super::set(None);
        log::warn!(target: "beavermail", "after it was turned off");
        let written = std::fs::read_to_string(super::path(&dir)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(written.contains("BeaverMail ") && written.contains("message 7: read in 2ms"));
        assert!(!written.contains("not wanted") && !written.contains("turned off"));
    }
}
