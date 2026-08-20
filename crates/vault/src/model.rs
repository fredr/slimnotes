//! Core vault data types.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A node in the vault file tree: either a folder (with `children`) or a
/// markdown note (a leaf). Paths are absolute so callers can read/write them
/// directly. This is a runtime-only view of the filesystem, so it carries no
/// serde derives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub children: Vec<Entry>,
}

/// Persisted user settings (stored as JSON in the app config dir).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Absolute path to the currently selected Obsidian vault, if any.
    pub vault: Option<PathBuf>,
    /// Per-vault sidebar state: vault path (as `to_string_lossy`) -> the folder
    /// paths the user has expanded. Folders default to collapsed, so anything
    /// not listed here (or a vault with no entry) is shown collapsed.
    #[serde(default)]
    pub expanded_folders: HashMap<String, Vec<String>>,
}
