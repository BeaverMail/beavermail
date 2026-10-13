//! Opening and saving the files attached to a message.
//!
//! An attachment is untrusted: its name is a suggestion (never a path), and some kinds of file run
//! code when opened, so those ask first.

use std::path::{Path, PathBuf};

/// A name that is safe to use as a file name on any system: no folders, no control characters, no
/// leading dots, nothing too long, never empty.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base.chars().map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim_end_matches(['.', ' ']);
    let cleaned: String = cleaned.chars().take(120).collect();
    // Names Windows reserves for devices.
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit()));
    if cleaned.is_empty() {
        "attachment".to_owned()
    } else if reserved {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// Files that run code, install software or carry macros: opening one asks first.
pub fn is_risky(name: &str) -> bool {
    const RISKY: [&str; 44] = [
        "exe", "msi", "bat", "cmd", "com", "scr", "pif", "vbs", "vbe", "js", "jse", "wsf", "wsh", "ps1", "psm1", "jar", "app", "dmg", "pkg", "deb", "rpm", "sh", "bash", "run", "appimage", "desktop", "lnk", "reg",
        "hta", "cpl", "apk", "iso", "dll", "sys", "docm", "xlsm", "pptm", "dotm", "xlam", "ps2", "gadget", "inf", "py", "pl",
    ];
    name.rsplit_once('.').is_some_and(|(_, ext)| RISKY.contains(&ext.to_ascii_lowercase().as_str()))
}

/// "1.2 MB", "340 KB", "12 B".
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

/// `name`, or `name (1).ext`, `name (2).ext`… so nothing already there is overwritten.
pub fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map_or((name, String::new()), |(s, e)| (s, format!(".{e}")));
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// The media type for a file name, from its extension; plain bytes when it is not known.
pub fn guess_mime(name: &str) -> String {
    let ext = name.rsplit_once('.').map_or(String::new(), |(_, e)| e.to_ascii_lowercase());
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "ics" => "text/calendar",
        "vcf" => "text/vcard",
        _ => "application/octet-stream",
    }
    .to_owned()
}

/// Reads the files to send. Run it off the UI thread: they can be large.
pub fn read_files(files: &[(PathBuf, String)]) -> Result<Vec<beavermail_core::OutgoingFile>, String> {
    files
        .iter()
        .map(|(path, name)| {
            let data = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
            Ok(beavermail_core::OutgoingFile { name: name.clone(), mime: guess_mime(name), data })
        })
        .collect()
}

/// Writes a private copy under `temp_root` and opens it with the system's default program.
pub fn open(temp_root: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = temp_root.join(format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    restrict(temp_root);
    restrict(&dir);
    let path = dir.join(safe_name(name));
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    opener::open(&path).map_err(|e| format!("could not open {}: {e}", path.display()))
}

#[cfg(unix)]
fn restrict(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_reduced_to_a_harmless_file_name() {
        assert_eq!(safe_name("plan.pdf"), "plan.pdf");
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("a<b>c:d|e?.txt"), "a_b_c_d_e_.txt");
        assert_eq!(safe_name("report.pdf  "), "report.pdf");
        assert_eq!(safe_name(""), "attachment");
        assert_eq!(safe_name(".."), "attachment");
        assert_eq!(safe_name("NUL.txt"), "_NUL.txt");
        assert_eq!(safe_name("com1"), "_com1");
        assert_eq!(safe_name(&"x".repeat(300)).len(), 120);
        assert_eq!(safe_name("bad\u{0}name.txt"), "bad_name.txt");
    }

    #[test]
    fn programs_and_macro_documents_are_risky_pictures_and_pdfs_are_not() {
        for risky in ["setup.EXE", "run.sh", "invoice.docm", "Tool.AppImage", "a.b.js", "x.desktop"] {
            assert!(is_risky(risky), "{risky}");
        }
        for fine in ["plan.pdf", "photo.jpg", "notes.txt", "exe", "archive.zip", "sheet.xlsx"] {
            assert!(!is_risky(fine), "{fine}");
        }
    }

    #[test]
    fn media_types_follow_the_extension_and_default_to_plain_bytes() {
        assert_eq!(guess_mime("plan.PDF"), "application/pdf");
        assert_eq!(guess_mime("photo.jpeg"), "image/jpeg");
        assert_eq!(guess_mime("data.unknown"), "application/octet-stream");
        assert_eq!(guess_mime("noextension"), "application/octet-stream");
    }

    #[test]
    fn files_are_read_with_their_name_and_a_missing_one_is_reported_by_name() {
        let dir = std::env::temp_dir().join(format!("beavermail-send-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let read = read_files(&[(dir.join("a.txt"), "a.txt".into())]).unwrap();
        assert_eq!((read[0].name.as_str(), read[0].mime.as_str(), read[0].data.as_slice()), ("a.txt", "text/plain", &b"hello"[..]));
        let error = read_files(&[(dir.join("gone.pdf"), "gone.pdf".into())]).unwrap_err();
        assert!(error.starts_with("gone.pdf:"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!([12, 340 * 1024, 1_300_000, 3 << 30].map(human_size), ["12 B", "340 KB", "1.2 MB", "3.0 GB"]);
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("beavermail-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plan.pdf"), b"1").unwrap();
        std::fs::write(dir.join("plan (1).pdf"), b"2").unwrap();
        assert_eq!(unique(&dir, "plan.pdf"), dir.join("plan (2).pdf"));
        assert_eq!(unique(&dir, "other.pdf"), dir.join("other.pdf"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
