//! `slimnotes-vault` owns the vault data model and all filesystem access for
//! slimnotes, an Obsidian-vault-compatible notes app.
//!
//! The filesystem is the live source of truth: the tree, notes, and settings
//! are read straight from disk, writes are atomic, a [`VaultWatcher`] reports
//! external changes, and [`file_sig`] detects edits made under an open note.
//!
//! All I/O here is synchronous; the app is expected to call it on a background
//! executor.

mod fs;
mod index;
mod model;
mod settings;
mod watch;

pub use fs::{FileSig, file_sig, read_note, read_tree, write_note};
pub use index::VaultIndex;
pub use model::{Entry, Settings};
pub use settings::{config_path, load_settings, save_settings};
pub use watch::{VaultEvent, VaultWatcher, watch};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use std::path::{Path, PathBuf};

    /// Create a directory (and parents), panicking on failure — test helper.
    fn mkdir(p: &Path) {
        stdfs::create_dir_all(p).unwrap();
    }
    fn touch(p: &Path, contents: &str) {
        if let Some(parent) = p.parent() {
            stdfs::create_dir_all(parent).unwrap();
        }
        stdfs::write(p, contents).unwrap();
    }

    #[test]
    fn read_tree_structure_sort_and_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Folders (out of order) + notes + a hidden dir + a non-md file.
        mkdir(&root.join("Zeta"));
        mkdir(&root.join("alpha"));
        mkdir(&root.join(".obsidian"));
        touch(&root.join(".obsidian/config.json"), "{}");
        touch(&root.join("alpha/nested.md"), "n");
        touch(&root.join("Beta.md"), "b");
        touch(&root.join("apple.md"), "a");
        touch(&root.join("notes.txt"), "not markdown");

        let tree = read_tree(root);

        // Dirs first (case-insensitive: alpha < Zeta), then files
        // (apple < Beta). Hidden dir and .txt excluded.
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Zeta", "apple.md", "Beta.md"]);

        let alpha = &tree[0];
        assert!(alpha.is_dir);
        assert_eq!(alpha.path, root.join("alpha"));
        assert_eq!(alpha.children.len(), 1);
        assert_eq!(alpha.children[0].name, "nested.md");
        assert!(!alpha.children[0].is_dir);

        // No hidden entry anywhere at the top level.
        assert!(tree.iter().all(|e| e.name != ".obsidian"));
    }

    #[test]
    fn write_note_atomic_and_no_leftover_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub").join("note.md");

        write_note(&path, "hello").unwrap();
        assert_eq!(read_note(&path).unwrap(), "hello");

        // Overwrite works.
        write_note(&path, "world again").unwrap();
        assert_eq!(read_note(&path).unwrap(), "world again");

        // No leftover temp files in the directory.
        let leftovers: Vec<PathBuf> = stdfs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(".slimnotes-tmp-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn file_sig_detects_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("note.md");
        assert!(file_sig(&path).is_none());

        write_note(&path, "abc").unwrap();
        let sig = file_sig(&path).unwrap();
        assert_eq!(sig.len, 3);

        write_note(&path, "abcdef").unwrap();
        let sig2 = file_sig(&path).unwrap();
        assert_eq!(sig2.len, 6);
        assert_ne!(sig, sig2);
    }

    #[test]
    fn resolve_note_relative_basename_and_stripping() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        touch(&root.join("Projects/Ideas.md"), "");
        touch(&root.join("Daily/Ideas.md"), "");
        touch(&root.join("Root Note.md"), "");
        touch(&root.join(".obsidian/skip.md"), "");

        let index = VaultIndex::build(root);

        // Vault-relative exact path wins even though a shallower basename match
        // exists elsewhere.
        assert_eq!(
            index.resolve_note("Daily/Ideas"),
            Some(root.join("Daily/Ideas.md"))
        );

        // Basename match, shortest path wins (both are depth 2 here, so either
        // is acceptable — assert it resolves to one of them).
        let got = index.resolve_note("Ideas").unwrap();
        assert!(got == root.join("Projects/Ideas.md") || got == root.join("Daily/Ideas.md"));

        // Top-level note by basename.
        assert_eq!(
            index.resolve_note("Root Note"),
            Some(root.join("Root Note.md"))
        );

        // `.md` suffix and `#heading` are stripped.
        assert_eq!(
            index.resolve_note("Root Note.md"),
            Some(root.join("Root Note.md"))
        );
        assert_eq!(
            index.resolve_note("Root Note#Section"),
            Some(root.join("Root Note.md"))
        );

        // Missing note.
        assert_eq!(index.resolve_note("Nope"), None);
        assert_eq!(index.resolve_note(""), None);
        // Hidden dir is not indexed.
        assert_eq!(index.resolve_note("skip"), None);
    }

    #[test]
    fn resolve_note_shortest_path_tiebreak() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a/b/Deep.md"), "");
        touch(&root.join("Shallow.md"), "");
        // Same basename at different depths.
        touch(&root.join("Target.md"), "");
        touch(&root.join("x/y/Target.md"), "");

        let index = VaultIndex::build(root);
        assert_eq!(index.resolve_note("Target"), Some(root.join("Target.md")));
    }

    #[test]
    fn resolve_image_basename_and_shortest_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("assets/Pic.PNG"), "");
        touch(&root.join("deep/nested/pic.png"), "");
        touch(&root.join("logo.jpg"), "");

        let index = VaultIndex::build(root);

        // Case-insensitive filename match, shortest path wins.
        assert_eq!(
            index.resolve_image("pic.png"),
            Some(root.join("assets/Pic.PNG"))
        );
        assert_eq!(index.resolve_image("logo.jpg"), Some(root.join("logo.jpg")));
        // Path-like query resolves by its filename component.
        assert_eq!(
            index.resolve_image("somewhere/logo.jpg"),
            Some(root.join("logo.jpg"))
        );
        assert_eq!(index.resolve_image("missing.gif"), None);
    }

    #[test]
    fn settings_round_trip() {
        let s = Settings {
            vault: Some(PathBuf::from("/home/user/vault")),
            expanded_folders: [(
                "/home/user/vault".to_string(),
                vec!["/home/user/vault/folder".to_string()],
            )]
            .into_iter()
            .collect(),
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn settings_bad_json_backs_up_and_defaults() {
        // Point the config dir at a temp location via XDG_CONFIG_HOME.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test; sets an env var for this process.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }

        let path = config_path().expect("config path");
        touch(&path, "this is not json {{{");

        let loaded = load_settings();
        assert_eq!(loaded, Settings::default());

        // Original renamed to .bak, original gone.
        assert!(!path.exists(), "bad settings file should be renamed away");
        assert!(
            path.with_extension("json.bak").exists(),
            "backup file should exist"
        );

        // A valid save then loads cleanly.
        let s = Settings {
            vault: Some(PathBuf::from("/tmp/v")),
            ..Default::default()
        };
        save_settings(&s).unwrap();
        assert_eq!(load_settings(), s);
    }
}
