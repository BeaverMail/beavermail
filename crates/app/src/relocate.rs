// SPDX-License-Identifier: AGPL-3.0-only

//! Moving the data folder (archive, message files, keys) to another place. It happens at start,
//! before anything in it is open: the new place is taken into use only once everything is there, and
//! the old one is removed last, so an interruption at any point loses nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Left in the new folder while it is being filled: one found there at start is an unfinished copy.
const MARKER: &str = ".beavermail-moving";

/// A move the user can confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub to: PathBuf,
    /// What is in the data folder now.
    pub bytes: u64,
    /// On the same disk the folder is renamed, at once and with no room needed.
    pub same_disk: bool,
}

/// Whether `to` can take the data folder `from`: a folder of its own (new or empty), not inside
/// `from`, writable, on a disk with room for it.
pub fn check(from: &Path, to: &Path) -> Result<Plan, String> {
    if !to.is_absolute() {
        return Err("Choose a full path".into());
    }
    if to == from {
        return Err("The data is already there".into());
    }
    if to.starts_with(from) {
        return Err("Choose a folder outside the current one".into());
    }
    if to.exists() && std::fs::read_dir(to).map_err(|e| e.to_string())?.next().is_some() {
        return Err("Choose an empty folder, or a new one: BeaverMail does not mix its data with other files".into());
    }
    let existing = nearest(to).ok_or("That disk cannot be reached")?;
    let probe = existing.join(".beavermail-probe");
    std::fs::write(&probe, b"").map_err(|e| format!("BeaverMail cannot write there: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    let bytes = size(from);
    let same_disk = same_disk(from, &existing);
    if !same_disk && let Some(free) = free_space(&existing) && free < bytes + bytes / 20 {
        return Err(format!("Needs {}, {} free on that disk", amount(bytes), amount(free)));
    }
    Ok(Plan { to: to.to_owned(), bytes, same_disk })
}

/// Bytes as people read them: "123 MB".
pub fn amount(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / f64::from(1u32 << 30)),
        b if b >= 1 << 20 => format!("{} MB", b >> 20),
        b => format!("{} KB", (b >> 10).max(1)),
    }
}

/// The bytes copied so far and in all, for the screen shown while moving.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

/// Moves `from` to `to`, calling `taken` once `to` is complete and before `from` is removed: that is
/// when the new place must be saved as the one in use. Started again after an interruption, it goes
/// on from where it stopped.
pub fn run(from: &Path, to: &Path, progress: &Arc<Progress>, taken: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let io = |what: &Path, e: std::io::Error| format!("{}: {e}", what.display());
    if !from.exists() && to.exists() && !to.join(MARKER).exists() {
        return taken(); // renamed already, stopped before it was saved
    }
    if to.join(MARKER).exists() {
        clear(to).map_err(|e| io(to, e))?; // an unfinished copy: started again
    }
    if !from.exists() {
        return Err(format!("{} is not there any more", from.display()));
    }
    // An empty folder in the way of a rename is removed; one that is not empty means a copy.
    if to.exists() {
        std::fs::remove_dir(to).map_err(|e| io(to, e))?;
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    if std::fs::rename(from, to).is_ok() {
        // Not saved as the new place: back where it was, where the next start looks.
        return taken().inspect_err(|_| {
            let _ = std::fs::rename(to, from);
        });
    }
    progress.total.store(size(from), Ordering::Relaxed);
    let copied = (|| {
        std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
        std::fs::write(to.join(MARKER), b"").map_err(|e| io(to, e))?;
        copy(from, to, progress).map_err(|(p, e)| io(&p, e))?;
        taken()?;
        std::fs::remove_file(to.join(MARKER)).map_err(|e| io(to, e))
    })();
    if let Err(e) = copied {
        // Nothing half-made stays behind: the data goes on from where it was.
        let _ = std::fs::remove_dir_all(to);
        return Err(e);
    }
    // The old copy is no longer read; one that cannot be removed only takes room.
    std::fs::remove_dir_all(from).map_err(|e| format!("Moved, but the old folder could not be removed: {}", io(from, e)))
}

fn copy(from: &Path, to: &Path, progress: &Progress) -> Result<(), (PathBuf, std::io::Error)> {
    for entry in std::fs::read_dir(from).map_err(|e| (from.to_owned(), e))? {
        let entry = entry.map_err(|e| (from.to_owned(), e))?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().map_err(|e| (source.clone(), e))?.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| (target.clone(), e))?;
            copy(&source, &target, progress)?;
        } else {
            let n = std::fs::copy(&source, &target).map_err(|e| (source.clone(), e))?;
            progress.done.fetch_add(n, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Empties `dir`, keeping it.
fn clear(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() { std::fs::remove_dir_all(&path)? } else { std::fs::remove_file(&path)? }
    }
    Ok(())
}

/// The bytes of every file under `dir`.
fn size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| match e.file_type() {
        Ok(t) if t.is_dir() => size(&e.path()),
        _ => e.metadata().map_or(0, |m| m.len()),
    }).sum()
}

/// `path`, or the closest folder above it that exists.
fn nearest(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.is_dir()).map(Path::to_owned)
}

#[cfg(unix)]
fn same_disk(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(a), Ok(b)) if a.dev() == b.dev())
}

/// shortcut: the same drive letter is taken for the same disk, which a folder mounted inside a drive
/// is not; the rename then fails and the data is copied instead, with no room checked.
#[cfg(windows)]
fn same_disk(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| p.components().next().map(|c| c.as_os_str().to_ascii_lowercase());
    root(a) == root(b)
}

fn free_space(dir: &Path) -> Option<u64> {
    fs4::available_space(dir).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beavermail-relocate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fill(dir: &Path) {
        std::fs::create_dir_all(dir.join("mail/INBOX")).unwrap();
        std::fs::write(dir.join("mail.sqlite3"), b"index").unwrap();
        std::fs::write(dir.join("mail/INBOX/1"), b"message").unwrap();
    }

    #[test]
    fn a_folder_must_be_new_or_empty_and_outside() {
        let base = temp("check");
        let from = base.join("data");
        fill(&from);
        assert!(check(&from, &from).is_err());
        assert!(check(&from, &from.join("inside")).is_err(), "inside the current one");
        assert!(check(&from, Path::new("relative")).is_err());
        std::fs::create_dir_all(base.join("busy")).unwrap();
        std::fs::write(base.join("busy/other"), b"x").unwrap();
        assert!(check(&from, &base.join("busy")).is_err(), "not empty");
        let plan = check(&from, &base.join("new/place")).unwrap();
        assert_eq!(plan.bytes, 12);
        assert!(plan.same_disk);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn everything_arrives_and_the_old_folder_goes() {
        let base = temp("run");
        let (from, to) = (base.join("data"), base.join("elsewhere/data"));
        fill(&from);
        let mut saved = false;
        run(&from, &to, &Arc::default(), || {
            saved = true;
            Ok(())
        })
        .unwrap();
        assert!(saved);
        assert_eq!(std::fs::read(to.join("mail/INBOX/1")).unwrap(), b"message");
        assert!(!from.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_copy_is_made_again_after_an_interruption_and_undone_on_failure() {
        let base = temp("again");
        let (from, to) = (base.join("data"), base.join("new"));
        fill(&from);
        // What an interrupted copy leaves: the marker and part of the files.
        std::fs::create_dir_all(to.join("mail")).unwrap();
        std::fs::write(to.join(MARKER), b"").unwrap();
        std::fs::write(to.join("mail.sqlite3"), b"ind").unwrap();
        // Failing when the new place is to be saved: nothing new stays, the data is where it was.
        assert!(run(&from, &to, &Arc::default(), || Err("no".into())).is_err());
        assert!(!to.exists() || std::fs::read_dir(&to).unwrap().next().is_none());
        assert_eq!(std::fs::read(from.join("mail.sqlite3")).unwrap(), b"index");
        // Renamed, but stopped before the new place was saved: saved now.
        std::fs::rename(&from, &to).unwrap();
        let mut saved = false;
        run(&from, &to, &Arc::default(), || {
            saved = true;
            Ok(())
        })
        .unwrap();
        assert!(saved);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn amounts_read_as_people_say_them() {
        assert_eq!(amount(500), "1 KB");
        assert_eq!(amount(123 << 20), "123 MB");
        assert_eq!(amount(3 << 29), "1.5 GB");
    }
}
