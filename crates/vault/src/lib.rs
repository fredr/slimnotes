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

pub use fs::{
    FileSig, create_folder, create_note, file_sig, move_entry, note_paths, read_note, read_tree,
    rename_entry, trash_entry, write_note,
};
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

    /// The note finder matches against this list, so its order is the sidebar's
    /// order and its contents are every note in the vault — including notes in
    /// folders the user has never expanded, since expansion is only a filter on
    /// what the tree *shows*.
    #[test]
    fn note_paths_flattens_every_note_in_tree_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        mkdir(&root.join(".obsidian"));
        touch(&root.join(".obsidian/config.json"), "{}");
        touch(&root.join("Projects/deep/Ideas.md"), "i");
        touch(&root.join("Projects/Alpha.md"), "a");
        touch(&root.join("Welcome.md"), "w");
        touch(&root.join("notes.txt"), "not markdown");
        // An empty folder contributes no candidates at all.
        mkdir(&root.join("Empty"));

        let paths = note_paths(&read_tree(root), root);

        // Folders first at each level (Empty < Projects), recursed before that
        // level's own files — so the deepest note comes first and the root
        // note last. Hidden dirs and non-md files are absent, as is the folder
        // itself: only notes can be opened.
        assert_eq!(
            paths,
            vec![
                PathBuf::from("Projects/deep/Ideas.md"),
                PathBuf::from("Projects/Alpha.md"),
                PathBuf::from("Welcome.md"),
            ]
        );
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
    fn create_note_and_folder_refuse_to_clobber() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        create_note(&root.join("New.md")).unwrap();
        assert_eq!(read_note(&root.join("New.md")).unwrap(), "");
        // A second create must fail rather than truncate the user's note.
        touch(&root.join("Taken.md"), "precious");
        assert_eq!(
            create_note(&root.join("Taken.md")).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(read_note(&root.join("Taken.md")).unwrap(), "precious");

        create_folder(&root.join("Folder")).unwrap();
        assert!(root.join("Folder").is_dir());
        // Unlike create_dir_all, a second create is an error.
        assert_eq!(
            create_folder(&root.join("Folder")).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        // A folder name taken by a note is also a collision.
        assert!(create_folder(&root.join("Taken.md")).is_err());
    }

    #[test]
    fn rename_entry_moves_and_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("Old.md"), "body");
        touch(&root.join("Other.md"), "other body");

        rename_entry(&root.join("Old.md"), &root.join("New.md")).unwrap();
        assert!(!root.join("Old.md").exists());
        assert_eq!(read_note(&root.join("New.md")).unwrap(), "body");

        // Renaming onto an existing note must not destroy it — plain fs::rename
        // would silently replace it on Unix.
        let err = rename_entry(&root.join("New.md"), &root.join("Other.md")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(read_note(&root.join("Other.md")).unwrap(), "other body");
        assert_eq!(read_note(&root.join("New.md")).unwrap(), "body");

        // Renaming to itself is a no-op, not an "already exists" error.
        rename_entry(&root.join("New.md"), &root.join("New.md")).unwrap();
        assert_eq!(read_note(&root.join("New.md")).unwrap(), "body");

        // Folders rename with their contents.
        touch(&root.join("Dir/Nested.md"), "nested");
        rename_entry(&root.join("Dir"), &root.join("Renamed")).unwrap();
        assert_eq!(read_note(&root.join("Renamed/Nested.md")).unwrap(), "nested");
    }

    #[test]
    fn move_entry_into_folder_and_rejects_moving_into_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("Note.md"), "n");
        mkdir(&root.join("Target"));
        touch(&root.join("Parent/Child/Deep.md"), "d");

        let moved = move_entry(&root.join("Note.md"), &root.join("Target")).unwrap();
        assert_eq!(moved, root.join("Target/Note.md"));
        assert_eq!(read_note(&moved).unwrap(), "n");

        // Moving a folder into its own descendant would detach the subtree.
        let err = move_entry(&root.join("Parent"), &root.join("Parent/Child")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(root.join("Parent/Child/Deep.md").exists());
        // ...and so would moving it into itself.
        assert!(move_entry(&root.join("Parent"), &root.join("Parent")).is_err());

        // A name already taken in the destination is an error, not a clobber.
        touch(&root.join("Note.md"), "different");
        let err = move_entry(&root.join("Note.md"), &root.join("Target")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(read_note(&root.join("Target/Note.md")).unwrap(), "n");
    }

    #[test]
    fn trash_entry_moves_to_dot_trash_and_uniquifies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("Ideas.md"), "first");
        touch(&root.join("Sub/Ideas.md"), "second");
        mkdir(&root.join("Folder"));

        let t1 = trash_entry(root, &root.join("Ideas.md")).unwrap();
        assert_eq!(t1, root.join(".trash/Ideas.md"));
        assert_eq!(read_note(&t1).unwrap(), "first");
        assert!(!root.join("Ideas.md").exists());

        // A second note of the same name keeps both, extension preserved.
        let t2 = trash_entry(root, &root.join("Sub/Ideas.md")).unwrap();
        assert_eq!(t2, root.join(".trash/Ideas 1.md"));
        assert_eq!(read_note(&t2).unwrap(), "second");

        // Folders trash too, and have no extension to preserve.
        assert_eq!(
            trash_entry(root, &root.join("Folder")).unwrap(),
            root.join(".trash/Folder")
        );
        mkdir(&root.join("Folder"));
        assert_eq!(
            trash_entry(root, &root.join("Folder")).unwrap(),
            root.join(".trash/Folder 1")
        );

        // Trashed notes are hidden from the tree, so nothing extra to filter.
        assert!(read_tree(root).iter().all(|e| e.name != ".trash"));
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
            last_note: [(
                "/home/user/vault".to_string(),
                "/home/user/vault/folder/Note.md".to_string(),
            )]
            .into_iter()
            .collect(),
            recent_notes: [(
                "/home/user/vault".to_string(),
                vec![
                    "/home/user/vault/folder/Note.md".to_string(),
                    "/home/user/vault/Other.md".to_string(),
                ],
            )]
            .into_iter()
            .collect(),
            sidebar_width: Some(280.0),
            sidebar_collapsed: true,
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    /// A settings file written before a field existed must still load: the app
    /// would otherwise back up the user's settings and reset to defaults on
    /// upgrade, losing their vault.
    #[test]
    fn settings_from_an_older_version_load_with_defaults() {
        let json = r#"{ "vault": "/home/user/vault" }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.vault, Some(PathBuf::from("/home/user/vault")));
        assert!(s.expanded_folders.is_empty());
        assert!(s.last_note.is_empty());
        assert_eq!(s.sidebar_width, None);
        assert!(!s.sidebar_collapsed);
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
