//! Image references on a line and loading/resolving them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{ImageFormat, RenderImage};

/// An image referenced on its own line: a markdown path or an Obsidian embed.
pub(crate) enum ImageRef {
    Rel(String),   // ![alt](path) — relative to the note folder
    Embed(String), // ![[name]] — resolved vault-wide by filename
}

/// If a line is solely an image — markdown `![alt](path)` or Obsidian
/// `![[name.png]]` (optionally `![[name.png|size]]`) — return its reference.
pub(crate) fn parse_image_line(line: &str) -> Option<ImageRef> {
    let t = line.trim();
    // Obsidian embed.
    if let Some(rest) = t.strip_prefix("![[") {
        let inner = rest.strip_suffix("]]")?;
        let name = inner.split('|').next()?.trim(); // drop |size / |alt
        return (!name.is_empty()).then(|| ImageRef::Embed(name.to_string()));
    }
    // Markdown image.
    let t = t.strip_prefix("![")?;
    let close = t.find("](")?;
    let inside = t[close + 2..].strip_suffix(')')?;
    let path = inside.split_whitespace().next()?; // drop optional "title"
    if path.is_empty() || path.starts_with("http://") || path.starts_with("https://") {
        return None;
    }
    Some(ImageRef::Rel(path.to_string()))
}

/// The image file types the editor can decode and draw. Doubles as the filter
/// for files dragged onto a note, so what is accepted is what will render.
const IMAGE_EXTS: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// Whether `path` names one of those.
pub(crate) fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
}

/// The file extension to store a clipboard image under, or `None` for a format
/// the editor can't draw — better to leave such a paste alone than to write a
/// file into the vault and embed it as a line that renders as raw text.
pub(crate) fn clipboard_image_ext(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        // The `image` crate has no SVG decoder.
        ImageFormat::Svg => return None,
    })
}

/// Walk the vault once, mapping each image file's lowercased basename to its
/// path (shortest path wins on collision, matching Obsidian's preference).
pub(crate) fn build_vault_index(vault: Option<&Path>) -> HashMap<String, PathBuf> {
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    let Some(root) = vault else { return map };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
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
            if !is_image_path(&path) {
                continue;
            }
            let key = name.to_lowercase();
            let shorter = map
                .get(&key)
                .map(|old| path.components().count() < old.components().count())
                .unwrap_or(true);
            if shorter {
                map.insert(key, path);
            }
        }
    }
    map
}

/// Read + decode an image file into a GPUI `RenderImage` (BGRA frames).
pub(crate) fn load_image(path: &Path) -> Option<Arc<RenderImage>> {
    let mut rgba = image::open(path).ok()?.into_rgba8();
    // GPUI expects BGRA; swap the R and B channels in place.
    for px in rgba.pixels_mut() {
        px.0.swap(0, 2);
    }
    let frame = image::Frame::new(rgba);
    Some(Arc::new(RenderImage::new(vec![frame])))
}
