// Hide the extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::{Path, PathBuf};

use slimnotes_core::{Entry, Settings};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

/// Location of the persisted settings file inside the OS app-config dir.
fn settings_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("no config dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

fn write_settings(app: &tauri::AppHandle, settings: &Settings) {
    if let Ok(path) = settings_path(app) {
        if let Ok(json) = serde_json::to_string_pretty(settings) {
            let _ = fs::write(path, json);
        }
    }
}

#[tauri::command]
fn load_settings(app: tauri::AppHandle) -> Settings {
    settings_path(&app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Open a native folder picker; on selection, remember it as the vault.
#[tauri::command]
fn pick_vault(app: tauri::AppHandle) -> Option<String> {
    let chosen = app.dialog().file().blocking_pick_folder()?;
    let path = chosen.into_path().ok()?.to_string_lossy().into_owned();
    write_settings(&app, &Settings { vault: Some(path.clone()) });
    Some(path)
}

/// Recursively read the vault into a sorted tree of folders and `.md` files.
/// Hidden entries (e.g. `.obsidian`) are skipped.
#[tauri::command]
fn read_tree(vault: String) -> Result<Vec<Entry>, String> {
    read_dir_entries(Path::new(&vault)).map_err(|e| e.to_string())
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

#[tauri::command]
fn read_note(path: String) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn write_note(path: String, content: String) -> Result<(), String> {
    fs::write(&path, content).map_err(|e| e.to_string())
}

/// Create a new empty markdown note under `vault`, avoiding name clashes by
/// appending a counter. Returns the absolute path of the created file.
#[tauri::command]
fn create_note(vault: String, name: String) -> Result<String, String> {
    let base = name.trim();
    let base = if base.is_empty() { "Untitled" } else { base };
    let base = base.trim_end_matches(".md");

    let dir = PathBuf::from(&vault);
    let mut candidate = dir.join(format!("{base}.md"));
    let mut n = 1;
    while candidate.exists() {
        n += 1;
        candidate = dir.join(format!("{base} {n}.md"));
    }
    fs::write(&candidate, "").map_err(|e| e.to_string())?;
    Ok(candidate.to_string_lossy().into_owned())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            load_settings,
            pick_vault,
            read_tree,
            read_note,
            write_note,
            create_note,
        ])
        .run(tauri::generate_context!())
        .expect("error while running slimnotes");
}
