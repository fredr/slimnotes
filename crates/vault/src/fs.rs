//! Synchronous filesystem access for the vault. All functions here are pure and
//! blocking; the app is expected to call them on a background executor.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
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
