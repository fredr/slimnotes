//! Loading and persisting [`Settings`] as JSON in the app config directory.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::model::Settings;

/// Path to the settings file: `<config_dir>/slimnotes/settings.json`.
/// `None` if the platform has no config dir.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("slimnotes").join("settings.json"))
}

/// Load persisted settings. A missing file yields defaults. A file that fails
/// to parse is *not* silently discarded: it is renamed to `settings.json.bak`
/// (best effort) so the user's data can be recovered, and defaults are returned.
pub fn load_settings() -> Settings {
    let Some(path) = config_path() else {
        return Settings::default();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Settings::default();
    };
    match serde_json::from_str(&text) {
        Ok(settings) => settings,
        Err(_) => {
            let _ = fs::rename(&path, path.with_extension("json.bak"));
            Settings::default()
        }
    }
}

/// Persist settings as pretty JSON, atomically (temp file in the same directory
/// then rename over the target). The parent directory is created if needed.
pub fn save_settings(settings: &Settings) -> io::Result<()> {
    let path = config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config directory"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    write_atomic(&path, json.as_bytes())
}

/// Write `bytes` to `path` atomically via a sibling temp file plus rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(".slimnotes-settings-tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
