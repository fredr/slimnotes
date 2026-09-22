//! Image references on a line and loading/resolving them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{ImageFormat, RenderImage};

/// An image referenced on its own line: a markdown path or an Obsidian embed.
#[derive(Debug, PartialEq)]
pub(crate) enum ImageRef {
    Rel(String),   // ![alt](path) — relative to the note folder
    Embed(String), // ![[name]] — resolved vault-wide by filename
}

/// The display size an embed asks for, in logical pixels. `height: None` is
/// Obsidian's `|width` form, where the height follows the aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ImageSize {
    pub(crate) width: f32,
    pub(crate) height: Option<f32>,
}

/// An image line: what to draw, and how big the note asks for it.
#[derive(Debug, PartialEq)]
pub(crate) struct ImageEmbed {
    pub(crate) target: ImageRef,
    pub(crate) size: Option<ImageSize>,
}

/// If a line is solely an image — markdown `![alt](path)` or Obsidian
/// `![[name.png]]` — return what it points at, plus any `|size` it carries.
pub(crate) fn parse_image_line(line: &str) -> Option<ImageEmbed> {
    let t = line.trim();
    // Obsidian embed: `![[name]]`, `![[name|300]]`, `![[name|alt|300x200]]`.
    if let Some(rest) = t.strip_prefix("![[") {
        let inner = rest.strip_suffix("]]")?;
        let (name, rest) = inner.split_once('|').unwrap_or((inner, ""));
        let name = name.trim();
        return (!name.is_empty()).then(|| ImageEmbed {
            target: ImageRef::Embed(name.to_string()),
            size: trailing_size(rest),
        });
    }
    // Markdown image. Obsidian puts the size in the alt text here — `![|300]`,
    // `![a cat|300x200]` — so the same trailing-segment rule applies.
    let t = t.strip_prefix("![")?;
    let close = t.find("](")?;
    let inside = t[close + 2..].strip_suffix(')')?;
    let path = inside.split_whitespace().next()?; // drop optional "title"
    if path.is_empty() || path.starts_with("http://") || path.starts_with("https://") {
        return None;
    }
    Some(ImageEmbed {
        target: ImageRef::Rel(path.to_string()),
        size: trailing_size(&t[..close]),
    })
}

/// The size in the last `|`-separated segment of `spec`, if that segment is one.
/// Everything before it — and a last segment that isn't a size — is alt text or
/// a caption, which this renderer has nowhere to put and so ignores.
fn trailing_size(spec: &str) -> Option<ImageSize> {
    let last = spec.rsplit('|').next()?.trim();
    // Obsidian writes `300` (width only) or `300x200`.
    let (w, h) = match last.split_once(['x', 'X']) {
        Some((w, h)) => (w, Some(h)),
        None => (last, None),
    };
    // A malformed height (`300xbig`) makes the whole thing alt text, not a
    // width of 300 — the note asked for something this doesn't understand.
    let height = match h {
        Some(h) => Some(dimension(h)?),
        None => None,
    };
    Some(ImageSize { width: dimension(w)?, height })
}

/// A positive whole number of pixels. Deliberately strict — anything else is
/// alt text that happens to sit in the last segment, not a size.
fn dimension(s: &str) -> Option<f32> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<f32>().ok().filter(|n| *n > 0.)
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
