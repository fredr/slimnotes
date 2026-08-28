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
///
/// `PartialEq` but not `Eq`: `sidebar_width` is a float.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// Absolute path to the currently selected Obsidian vault, if any.
    pub vault: Option<PathBuf>,
    /// Per-vault sidebar state: vault path (as `to_string_lossy`) -> the folder
    /// paths the user has expanded. Folders default to collapsed, so anything
    /// not listed here (or a vault with no entry) is shown collapsed.
    #[serde(default)]
    pub expanded_folders: HashMap<String, Vec<String>>,
    /// Per-vault last open note: vault path -> note path, so restarting (or
    /// switching back to a vault) reopens where the user left off. Keyed the
    /// same way as `expanded_folders`; an entry naming a note that no longer
    /// exists is ignored rather than treated as an error.
    #[serde(default)]
    pub last_note: HashMap<String, String>,
    /// Per-vault recently opened notes, most recent first: vault path -> note
    /// paths. What the note finder lists before anything is typed, so the note
    /// you were just in is one keystroke away.
    ///
    /// The first entry is the same note as `last_note`, deliberately: reopening
    /// on startup has to keep working for a settings file written before this
    /// field existed, so `last_note` stays the authority for that and this list
    /// is only an ordering. Entries naming notes that no longer exist are
    /// ignored by the reader rather than pruned here.
    #[serde(default)]
    pub recent_notes: HashMap<String, Vec<String>>,
    /// Sidebar width in pixels. Not per-vault: the sidebar is a property of the
    /// window, and a user who has sized it once means it for the app. `None`
    /// means "never resized", so the built-in default applies.
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// Whether the sidebar is collapsed.
    #[serde(default)]
    pub sidebar_collapsed: bool,
}
