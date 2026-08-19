//! Types shared between the Tauri backend and the Leptos frontend.
//!
//! These cross the IPC boundary as JSON, so they only need `serde`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A node in the vault file tree: either a folder (with `children`) or a
/// markdown note (a leaf). Paths are absolute so the backend can read/write
/// them directly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    #[serde(default)]
    pub children: Vec<Entry>,
}

/// Persisted user settings (stored as JSON in the app config dir).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Absolute path to the currently selected Obsidian vault, if any.
    pub vault: Option<String>,
    /// Per-vault sidebar state: vault path -> the folder paths the user has
    /// expanded. Folders default to collapsed, so anything not listed here (or
    /// for a vault with no entry) is shown collapsed.
    #[serde(default)]
    pub expanded_folders: HashMap<String, Vec<String>>,
}
