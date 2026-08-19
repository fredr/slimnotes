//! Vault access: reading the folder tree, notes, and persisted settings.
//! Everything runs in-process (no IPC), so this is just `std::fs`.

use std::fs;
use std::path::{Path, PathBuf};

use slimnotes_core::{Entry, Settings};

fn config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("slimnotes").join("settings.json"))
}

pub fn load_settings() -> Settings {
    config_file()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) {
    let Some(path) = config_file() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(path, json);
    }
}

/// Recursively read the vault into a sorted tree of folders and `.md` files.
/// Hidden entries (e.g. `.obsidian`) are skipped.
pub fn read_tree(root: &Path) -> Vec<Entry> {
    read_dir_entries(root).unwrap_or_default()
}

fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut dirs: Vec<Entry> = Vec::new();
    let mut files: Vec<Entry> = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            dirs.push(Entry {
                name,
                path: path.to_string_lossy().into_owned(),
                is_dir: true,
                children: read_dir_entries(&path)?,
            });
        } else if file_type.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("md")
        {
            files.push(Entry {
                name,
                path: path.to_string_lossy().into_owned(),
                is_dir: false,
                children: Vec::new(),
            });
        }
    }

    let by_name = |a: &Entry, b: &Entry| a.name.to_lowercase().cmp(&b.name.to_lowercase());
    dirs.sort_by(by_name);
    files.sort_by(by_name);
    dirs.extend(files);
    Ok(dirs)
}

/// Resolve an Obsidian `[[wikilink]]` name to a note path. Tries the vault-root
/// relative path first, then matches by basename anywhere in the vault (shortest
/// path wins), mirroring Obsidian's link resolution.
pub fn resolve_note(vault: &Path, name: &str) -> Option<PathBuf> {
    let want = name.trim().trim_end_matches(".md");
    if want.is_empty() {
        return None;
    }
    let direct = vault.join(format!("{want}.md"));
    if direct.is_file() {
        return Some(direct);
    }
    let want_base = want.rsplit(['/', '\\']).next().unwrap_or(want).to_lowercase();
    let mut best: Option<PathBuf> = None;
    let mut stack = vec![vault.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            if fname.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if stem == want_base
                    && best
                        .as_ref()
                        .is_none_or(|b| path.components().count() < b.components().count())
                {
                    best = Some(path);
                }
            }
        }
    }
    best
}

pub fn read_note(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

pub fn write_note(path: &Path, content: &str) -> std::io::Result<()> {
    fs::write(path, content)
}

