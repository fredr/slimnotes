//! Attachments: where an image pasted or dropped into a note is stored, and
//! putting it there without clobbering anything.
//!
//! The destination folder is a per-vault setting so it travels with the vault
//! rather than living in the app's global config. It is read from
//! `<vault>/.slimnotes/settings.json`, which slimnotes never writes — the file
//! is there for a user who wants to override a default for one vault, and its
//! absence simply means the defaults apply. The folder is hidden, so it stays
//! out of the note tree and the filesystem watcher for free.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::fs::write_atomic;

/// The in-vault config folder.
pub const CONFIG_DIR: &str = ".slimnotes";

/// Where attachments go when the vault says nothing. Obsidian's own default is
/// the vault root; a folder is used here so pasting an image doesn't scatter
/// files among the notes.
const DEFAULT_ATTACHMENT_DIR: &str = "attachments";

/// Per-vault settings. Unknown keys are ignored, so the file can carry settings
/// a future version adds without an older build rejecting it.
#[derive(Deserialize, Default)]
#[serde(default)]
struct VaultConfig {
    attachment_folder_path: Option<String>,
}

/// The folder new attachments for `note_dir` are written to.
///
/// `attachment_folder_path` is spelled the way Obsidian spells it, so a value
/// copied from an Obsidian vault means the same thing here:
///
/// | value        | folder                          |
/// |--------------|---------------------------------|
/// | `"/"` or `""`| the vault root                  |
/// | `"./"`       | the note's own folder           |
/// | `"./assets"` | `assets` beside the note        |
/// | `"assets"`   | `assets` under the vault root   |
pub fn attachment_dir(vault: &Path, note_dir: &Path) -> PathBuf {
    let spec = read_config(vault)
        .attachment_folder_path
        .unwrap_or_else(|| DEFAULT_ATTACHMENT_DIR.to_string());
    resolve_attachment_dir(&spec, vault, note_dir)
}

fn read_config(vault: &Path) -> VaultConfig {
    fs::read_to_string(vault.join(CONFIG_DIR).join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The path arithmetic behind [`attachment_dir`], split out so it can be tested
/// without a config file on disk.
fn resolve_attachment_dir(spec: &str, vault: &Path, note_dir: &Path) -> PathBuf {
    match spec.trim() {
        "" | "/" => vault.to_path_buf(),
        "." | "./" => note_dir.to_path_buf(),
        // "./sub" is relative to the note; everything else is read from the
        // vault root, where a leading "/" means the *vault* root and not the
        // filesystem's.
        s => match s.strip_prefix("./") {
            Some(rest) => note_dir.join(rest),
            None => vault.join(s.trim_start_matches('/')),
        },
    }
}

/// Write `bytes` into `dir` as `<stem>.<ext>`, creating the folder if needed.
/// A name that is already taken is stepped aside (`stem 1.ext`, `stem 2.ext`, …)
/// rather than overwritten: an attachment the user already has is data, and
/// nothing about pasting a new image says to destroy it. Returns the path
/// actually written.
pub fn save_attachment(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = free_path(dir, &sanitize_stem(stem), ext);
    write_atomic(&path, bytes)?;
    Ok(path)
}

/// Copy a file dragged in from outside the vault into `dir`, keeping its name.
/// Same non-clobbering rule as [`save_attachment`].
pub fn import_attachment(dir: &Path, src: &Path) -> io::Result<PathBuf> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "dropped file has no name"))?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, e),
        _ => (name, ""),
    };
    save_attachment(dir, stem, ext, &fs::read(src)?)
}

/// The name a clipboard image is filed under: `Pasted image 20260922101500`,
/// the same shape Obsidian uses, so a shared vault keeps one naming scheme.
///
/// The timestamp is UTC. Reading the machine's timezone would mean a new
/// dependency, and the name only has to be unique, sortable, and recognizable.
pub fn pasted_image_stem() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = utc_parts(secs);
    format!("Pasted image {y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}")
}

/// Split a Unix timestamp into UTC year, month, day, hour, minute, second
/// (Howard Hinnant's `civil_from_days`, which is exact for any day this can
/// produce and needs no tables).
fn utc_parts(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Shift the epoch to 0000-03-01 so leap days fall at the end of the "year".
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of that year
    let mp = (5 * doy + 2) / 153; // month, with March as 0
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (
        year,
        month,
        day,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Replace the characters that would either break an `![[embed]]` or make the
/// file awkward to move between filesystems. Runs of them collapse to one `-`.
fn sanitize_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|c| match c {
            '[' | ']' | '|' | '#' | '^' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(['-', ' ', '.']).to_string();
    if trimmed.is_empty() {
        "image".to_string()
    } else {
        trimmed
    }
}

/// The first free `<stem>.<ext>` in `dir`, counting up with a space-separated
/// suffix the way Obsidian and most file managers do.
fn free_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let name = |stem: &str| {
        if ext.is_empty() {
            stem.to_string()
        } else {
            format!("{stem}.{ext}")
        }
    };
    let first = dir.join(name(stem));
    if !first.exists() {
        return first;
    }
    for n in 1u32.. {
        let candidate = dir.join(name(&format!("{stem} {n}")));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 exhausted looking for a free attachment name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_dir_specs() {
        let vault = Path::new("/v");
        let note = Path::new("/v/Projects");
        let r = |s: &str| resolve_attachment_dir(s, vault, note);
        assert_eq!(r("/"), PathBuf::from("/v"));
        assert_eq!(r(""), PathBuf::from("/v"));
        assert_eq!(r("./"), PathBuf::from("/v/Projects"));
        assert_eq!(r("."), PathBuf::from("/v/Projects"));
        assert_eq!(r("./assets"), PathBuf::from("/v/Projects/assets"));
        assert_eq!(r("assets"), PathBuf::from("/v/assets"));
        assert_eq!(r("/files/img"), PathBuf::from("/v/files/img"));
        assert_eq!(r("  attachments  "), PathBuf::from("/v/attachments"));
    }

    #[test]
    fn attachment_dir_reads_vault_config() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        let note = vault.join("Projects");
        // No config file: the default folder under the vault root.
        assert_eq!(attachment_dir(vault, &note), vault.join("attachments"));

        fs::create_dir_all(vault.join(CONFIG_DIR)).unwrap();
        fs::write(
            vault.join(CONFIG_DIR).join("settings.json"),
            r#"{"attachment_folder_path": "./assets", "future_key": 1}"#,
        )
        .unwrap();
        assert_eq!(attachment_dir(vault, &note), note.join("assets"));

        // A corrupt file falls back to the default rather than failing a paste.
        fs::write(vault.join(CONFIG_DIR).join("settings.json"), "{not json").unwrap();
        assert_eq!(attachment_dir(vault, &note), vault.join("attachments"));
    }

    #[test]
    fn save_attachment_creates_dir_and_never_clobbers() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("attachments");

        let a = save_attachment(&dir, "shot", "png", b"one").unwrap();
        assert_eq!(a, dir.join("shot.png"));
        let b = save_attachment(&dir, "shot", "png", b"two").unwrap();
        assert_eq!(b, dir.join("shot 1.png"));
        let c = save_attachment(&dir, "shot", "png", b"three").unwrap();
        assert_eq!(c, dir.join("shot 2.png"));

        // The original is untouched.
        assert_eq!(fs::read(&a).unwrap(), b"one");
        assert_eq!(fs::read(&b).unwrap(), b"two");
    }

    #[test]
    fn import_attachment_copies_keeping_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("Downloads").join("diagram.png");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, b"bytes").unwrap();

        let dir = tmp.path().join("vault").join("attachments");
        let out = import_attachment(&dir, &src).unwrap();
        assert_eq!(out, dir.join("diagram.png"));
        assert_eq!(fs::read(&out).unwrap(), b"bytes");
        // Same file again: copied beside the first, not over it.
        assert_eq!(import_attachment(&dir, &src).unwrap(), dir.join("diagram 1.png"));
    }

    #[test]
    fn stems_are_safe_to_embed() {
        assert_eq!(sanitize_stem("plain name"), "plain name");
        assert_eq!(sanitize_stem("a[b]c|d#e^f"), "a-b-c-d-e-f");
        assert_eq!(sanitize_stem("../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_stem("|||"), "image");
    }

    #[test]
    fn pasted_stem_is_a_timestamp() {
        let stem = pasted_image_stem();
        let digits = stem.strip_prefix("Pasted image ").unwrap();
        assert_eq!(digits.len(), 14);
        assert!(digits.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn utc_parts_known_instants() {
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0));
        // 2026-09-22T10:15:00Z
        assert_eq!(utc_parts(1_790_072_100), (2026, 9, 22, 10, 15, 0));
        // A leap day, and the last second before a year rolls over.
        assert_eq!(utc_parts(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        assert_eq!(utc_parts(1_767_225_599), (2025, 12, 31, 23, 59, 59));
    }
}
