//! Filesystem watcher over the vault, so the on-disk state is the live source
//! of truth. Any change under the vault is coalesced into a single [`VaultEvent`]
//! signal; the app reacts by re-reading the tree and re-stat'ing the open note.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

/// A change somewhere under the watched vault. Deliberately coarse: creates,
/// removes, renames, and modifications all collapse to `Changed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultEvent {
    Changed,
}

/// Owns the underlying [`notify`] watcher so it stays alive. Dropping this stops
/// watching. Hold it for as long as you want change events.
pub struct VaultWatcher {
    _watcher: RecommendedWatcher,
}

/// Start watching `root` recursively. Returns the watcher (keep it alive) and a
/// receiver of coalesced [`VaultEvent`]s. Events under hidden directories (any
/// path component starting with `.`, e.g. `.git`, `.obsidian`) are ignored to
/// avoid noise.
pub fn watch(root: &Path) -> notify::Result<(VaultWatcher, Receiver<VaultEvent>)> {
    let (tx, rx) = mpsc::channel::<VaultEvent>();

    let handler = {
        let tx: Sender<VaultEvent> = tx;
        let root = root.to_path_buf();
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if event.paths.iter().all(|p| is_hidden_path(&root, p)) {
                    return;
                }
                // Ignore the send error: it just means the receiver was dropped.
                let _ = tx.send(VaultEvent::Changed);
            }
        }
    };

    let mut watcher = RecommendedWatcher::new(handler, Config::default())?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    Ok((VaultWatcher { _watcher: watcher }, rx))
}

/// True if `path` lies under a hidden directory *within* the vault, i.e. any
/// component of the path relative to `root` starts with `.`. The vault root's
/// own components are not considered (the vault may itself live under a dotdir).
fn is_hidden_path(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
    })
}
