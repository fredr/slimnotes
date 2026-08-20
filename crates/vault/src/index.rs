//! A one-pass index of a vault's notes and images, used by the wikilink
//! resolver, the tree, and the editor. Built once per vault; rebuild on change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Image extensions Obsidian embeds, matched case-insensitively.
const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// An index over a vault, built in a single filesystem walk. Maps lowercased
/// basenames to paths so `[[wikilink]]` and `![[image]]` references resolve
/// without re-walking the tree.
#[derive(Clone, Debug, Default)]
pub struct VaultIndex {
    /// The vault root, so relative-path resolution can be tried first.
    root: PathBuf,
    /// Lowercased note stem -> candidate paths (multiple on basename collision).
    notes: HashMap<String, Vec<PathBuf>>,
    /// Lowercased image filename -> path (shortest path wins on collision).
    images: HashMap<String, PathBuf>,
}

impl VaultIndex {
    /// Walk `root` once, indexing every `.md` note by stem and every image by
    /// filename. Hidden directories (name starting with `.`) are skipped.
    pub fn build(root: &Path) -> Self {
        let mut notes: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut images: HashMap<String, PathBuf> = HashMap::new();

        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase());
                match ext.as_deref() {
                    Some("md") => {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            notes.entry(stem.to_lowercase()).or_default().push(path);
                        }
                    }
                    Some(e) if IMAGE_EXTS.contains(&e) => {
                        let key = name.to_lowercase();
                        let shorter = images
                            .get(&key)
                            .map(|old| path.components().count() < old.components().count())
                            .unwrap_or(true);
                        if shorter {
                            images.insert(key, path);
                        }
                    }
                    _ => {}
                }
            }
        }

        VaultIndex {
            root: root.to_path_buf(),
            notes,
            images,
        }
    }

    /// Resolve an Obsidian `[[wikilink]]` name to a note path, mirroring
    /// Obsidian's resolution: try the vault-root relative path first, then match
    /// by basename anywhere in the vault (shortest path wins). A trailing `.md`
    /// and any `#heading` on the query are stripped.
    pub fn resolve_note(&self, name: &str) -> Option<PathBuf> {
        let want = name.trim();
        // Drop a `#heading` / `#^block` suffix, then a trailing `.md`.
        let want = want.split('#').next().unwrap_or(want).trim();
        let want = want.strip_suffix(".md").unwrap_or(want).trim();
        if want.is_empty() {
            return None;
        }

        // 1. Vault-relative exact path. Append `.md` (rather than replacing an
        // extension) so note names containing dots resolve correctly.
        let direct = self.root.join(format!("{want}.md"));
        if direct.is_file() {
            return Some(direct);
        }

        // 2. Basename match, shortest path wins.
        let base = want.rsplit(['/', '\\']).next().unwrap_or(want).to_lowercase();
        self.notes.get(&base).and_then(|paths| {
            paths
                .iter()
                .min_by_key(|p| p.components().count())
                .cloned()
        })
    }

    /// Resolve an embedded image name (`![[image.png]]`) to a path by lowercased
    /// filename, shortest path wins. Falls back to the filename component of a
    /// path-like query.
    pub fn resolve_image(&self, name: &str) -> Option<PathBuf> {
        let want = name.trim();
        let base = want.rsplit(['/', '\\']).next().unwrap_or(want).to_lowercase();
        self.images.get(&base).cloned()
    }
}
