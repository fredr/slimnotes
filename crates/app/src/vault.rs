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

pub fn read_note(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

pub fn write_note(path: &Path, content: &str) -> std::io::Result<()> {
    fs::write(path, content)
}

/// Split markdown into blocks on blank lines, keeping fenced code blocks intact.
pub fn split_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;

    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if line.trim().is_empty() && !in_fence {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    if blocks.is_empty() {
        blocks.push(String::new());
    }
    blocks
}

pub fn join_blocks(blocks: &[String]) -> String {
    let mut out = blocks.join("\n\n");
    out.push('\n');
    out
}
