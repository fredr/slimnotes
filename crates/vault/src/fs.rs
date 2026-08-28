//! Synchronous filesystem access for the vault. All functions here are pure and
//! blocking; the app is expected to call them on a background executor.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::model::Entry;

/// Recursively read the vault into a sorted tree of folders and `.md` files.
///
/// Ordering is folders first then files, each case-insensitively by name.
/// Hidden entries (name starting with `.`, e.g. `.obsidian`) are skipped, and
/// only `.md` files appear as leaves. An I/O error at any directory degrades
/// that subtree to empty rather than failing the whole tree.
pub fn read_tree(root: &Path) -> Vec<Entry> {
    read_dir_entries(root)
}

/// Every note in `tree`, as paths relative to `root`, in the depth-first order
/// [`read_tree`] sorted them — which is the order the sidebar shows.
///
/// Pure: [`read_tree`] already walks the whole vault (folders are recursed
/// unconditionally, and collapsing one in the UI only hides it), so flattening
/// its result costs no I/O. This is the candidate list the note finder matches
/// against. Paths are relative because that is what gets matched and shown; the
/// caller joins `root` back on to open one.
///
/// Folders are not included — only notes are openable. An entry whose path is
/// somehow not under `root` is skipped rather than reported: the tree is built
/// from `root`, so it cannot happen.
pub fn note_paths(tree: &[Entry], root: &Path) -> Vec<PathBuf> {
    fn walk(entries: &[Entry], root: &Path, out: &mut Vec<PathBuf>) {
        for entry in entries {
            if entry.is_dir {
                walk(&entry.children, root, out);
            } else if let Ok(rel) = entry.path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(tree, root, &mut out);
    out
}

fn read_dir_entries(dir: &Path) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut dirs: Vec<Entry> = Vec::new();
    let mut files: Vec<Entry> = Vec::new();

    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let children = read_dir_entries(&path);
            dirs.push(Entry {
                name,
                path,
                is_dir: true,
                children,
            });
        } else if file_type.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("md")
        {
            files.push(Entry {
                name,
                path,
                is_dir: false,
                children: Vec::new(),
            });
        }
    }

    let by_name = |a: &Entry, b: &Entry| a.name.to_lowercase().cmp(&b.name.to_lowercase());
    dirs.sort_by(by_name);
    files.sort_by(by_name);
    dirs.extend(files);
    dirs
}

/// Read a note's full contents as a UTF-8 string.
pub fn read_note(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
}

/// Write a note atomically: content is written to a temp file in the *same*
/// directory, flushed, then renamed over the target. This never truncates the
/// destination in place, so a crash mid-write can't corrupt the note. Failures
/// are returned so the caller can surface them.
pub fn write_note(path: &Path, content: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(".slimnotes-tmp-{}-{:?}", std::process::id(), unique()));
    // Best-effort cleanup on any error path below.
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Create a new empty note at `path`, refusing to touch anything that already
/// exists there. Unlike [`write_note`], which is the save path and must be able
/// to overwrite, this is the "new note" path: clobbering a note the user already
/// has would silently destroy it.
pub fn create_note(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Err(already_exists(path));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // create_new so two racing creations can't both think they won.
    fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(())
}

/// Create a folder, refusing to reuse an existing one. `create_dir_all` would
/// happily succeed on an existing directory; here the caller is naming a *new*
/// folder, so a name that's taken is an error the user needs to see.
pub fn create_folder(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Err(already_exists(path));
    }
    fs::create_dir_all(path)
}

/// Rename `from` to `to`, refusing to clobber an existing `to`.
///
/// `fs::rename` silently replaces the destination on Unix, which would destroy a
/// note by renaming another one onto it. The existence check is not atomic with
/// the rename, but it turns the common case (the user picks a name that's taken)
/// into a clear error instead of data loss.
pub fn rename_entry(from: &Path, to: &Path) -> io::Result<()> {
    if from == to {
        return Ok(());
    }
    // Compare case-insensitively too: on a case-insensitive filesystem a pure
    // case change ("note.md" -> "Note.md") has `to` "existing" as `from`.
    if to.exists() && !same_file(from, to) {
        return Err(already_exists(to));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)
}

/// Move `path` into `dest_dir`, keeping its file name. Returns the new path.
///
/// Refuses to move a folder into itself or into one of its own descendants —
/// that would detach the whole subtree from the vault.
pub fn move_entry(path: &Path, dest_dir: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| other_invalid(path))?;
    if dest_dir.starts_with(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot move {} into itself", path.display()),
        ));
    }
    let target = dest_dir.join(name);
    rename_entry(path, &target)?;
    Ok(target)
}

/// Move `path` into `<root>/.trash/`, the same place Obsidian puts deleted files
/// by default, and return where it landed. The name is uniquified on collision
/// so trashing two notes called `Ideas.md` keeps both.
///
/// `.trash` is hidden, so [`read_tree`] already skips it and trashed notes drop
/// out of the sidebar without any extra filtering.
pub fn trash_entry(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| other_invalid(path))?;
    let trash = root.join(".trash");
    fs::create_dir_all(&trash)?;

    let (stem, ext) = match name.rsplit_once('.') {
        // A leading dot is part of the name, not an extension separator.
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    for n in 0.. {
        let candidate = match (n, ext) {
            (0, _) => name.to_string(),
            (n, Some(ext)) => format!("{stem} {n}.{ext}"),
            (n, None) => format!("{stem} {n}"),
        };
        let target = trash.join(candidate);
        if !target.exists() {
            fs::rename(path, &target)?;
            return Ok(target);
        }
    }
    unreachable!("0.. is unbounded")
}

fn already_exists(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} already exists", path.display()),
    )
}

fn other_invalid(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} has no file name", path.display()),
    )
}

/// Whether two paths name the same file on disk. Used to let a rename that only
/// changes case through on a case-insensitive filesystem.
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A cheap fingerprint of a file's on-disk state, used to detect external edits
/// (e.g. the note changed under us while open in the editor).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileSig {
    pub mtime: SystemTime,
    pub len: u64,
}

/// Stat a file into a [`FileSig`]. Returns `None` if the file can't be stat'd
/// (missing, permission denied, or no mtime on this platform).
pub fn file_sig(path: &Path) -> Option<FileSig> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(FileSig {
        mtime,
        len: meta.len(),
    })
}

/// A process-local monotonic counter to make temp file names unique even for
/// rapid successive writes to the same path.
fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
