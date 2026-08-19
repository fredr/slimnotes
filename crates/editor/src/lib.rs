//! A from-scratch live-preview markdown editor built directly on GPUI.
//!
//! One editor over the whole note. Each line is shaped with its own font size
//! (real heading sizes) and wrap width, inline markdown is styled per span, and
//! the syntax markers are concealed on every line except the one the cursor is
//! on — the Obsidian "Live Preview" behavior.
//!
//! Supports typing, movement, selection, mouse, clipboard, undo/redo, soft-wrap
//! and scrolling.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use gpui::{
    anchored, deferred, div, fill, point, px, quad, relative, rgb, rgba, size, transparent_black, App,
    BorderStyle, Bounds, ClipboardItem, ContentMask, Context, Corners, DispatchPhase, Element,
    ElementId, Entity, EventEmitter, FocusHandle, Font, FontStyle, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, RenderImage,
    Rgba, ScrollDelta, ScrollWheelEvent, SharedString, Size, StrikethroughStyle, Style, Styled,
    TextAlign, TextRun, UnderlineStyle, WrappedLine, Window,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Emitted when the buffer content changes (so a host can persist it).
pub enum EditorEvent {
    Changed,
    /// A `[[wikilink]]` was clicked; the host should open the named note.
    OpenNote(String),
}

/// Where a clicked link points.
#[derive(Clone)]
enum LinkTarget {
    Url(SharedString),  // external `[text](url)`
    Note(SharedString), // internal `[[note]]`
}

const TEXT: u32 = 0xdcddde;
const BG: u32 = 0x1e1e1e;
const CARET: u32 = 0x9d86e9;
const LINK: u32 = 0x9d86e9;
const CODE_BG: u32 = 0x323232;
const CODE_PANEL: u32 = 0x282828;
const SELECTION: u32 = 0x4b4488;
const MUTED: u32 = 0x8a8a8a;
const ACCENT: u32 = 0x9d86e9;
const HIGHLIGHT_BG: u32 = 0xd6c04a; // ==highlight== background (highlighter yellow)
const HIGHLIGHT_FG: u32 = 0x1e1e1e; // dark text on the highlight
const TAG_BG: u32 = 0x9d86e933; // #tag pill background (translucent accent)

const UNDO_LIMIT: usize = 300;

const IMAGE_MAX_H: f32 = 400.0; // cap rendered image height (px)
const IMAGE_PAD: f32 = 6.0; // vertical breathing room around an image (px)
const QUOTE_INDENT: f32 = 22.0; // left indent per blockquote nesting level (px)

// Fonts are bundled (see `fonts/`) and registered at startup, so these families
// are always present and render identically without system fonts. GPUI's default
// `.SystemUIFont` doesn't resolve bold/italic faces, hence naming them directly.
const UI_FONT: &str = "IBM Plex Sans";
const MONO_FONT: &str = "IBM Plex Mono";

// Font sizes (px).
const BODY_FONT: f32 = 18.0; // body text + non-heading lines
const CODE_FONT: f32 = 16.0; // fenced code blocks + frontmatter

static FONTS_LOADED: std::sync::Once = std::sync::Once::new();

/// Register the bundled IBM Plex faces with the text system, once per process.
fn ensure_fonts(cx: &App) {
    FONTS_LOADED.call_once(|| {
        macro_rules! face {
            ($f:literal) => {
                std::borrow::Cow::Borrowed(&include_bytes!(concat!("../fonts/", $f))[..])
            };
        }
        let faces = vec![
            face!("IBMPlexSans-Regular.ttf"),
            face!("IBMPlexSans-Italic.ttf"),
            face!("IBMPlexSans-SemiBold.ttf"),
            face!("IBMPlexSans-Bold.ttf"),
            face!("IBMPlexSans-BoldItalic.ttf"),
            face!("IBMPlexMono-Regular.ttf"),
            face!("IBMPlexMono-Italic.ttf"),
            face!("IBMPlexMono-Bold.ttf"),
            face!("IBMPlexMono-BoldItalic.ttf"),
        ];
        let _ = cx.text_system().add_fonts(faces);
    });
}

/// A point-in-time buffer state for undo/redo.
#[derive(Clone)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

// ---------------------------------------------------------------------------
// The editor view.
// ---------------------------------------------------------------------------

/// A live-preview markdown editor. Create with [`MarkdownEditor::new`] and use
/// it as a GPUI view (it implements [`Render`]).
pub struct MarkdownEditor {
    text: String,
    cursor: usize,         // byte offset into `text`
    anchor: Option<usize>, // selection anchor; selection is anchor..cursor
    dragging: bool,
    scroll_top: Pixels,
    scroll_to_caret: bool,
    layout: Option<Rc<Layout>>, // last painted layout, for movement + hit-testing
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    last_coalesce: Option<u8>,
    focus_handle: FocusHandle,
    base_dir: Option<PathBuf>,  // note folder, for resolving relative image paths
    vault_dir: Option<PathBuf>, // vault root, for resolving ![[wikilink]] embeds
    image_cache: HashMap<PathBuf, Option<Arc<RenderImage>>>, // None = failed to load
    vault_index: Option<HashMap<String, PathBuf>>, // lazy basename -> image path map
    // Perf caches. `text_version` bumps on every text mutation; the parse and the
    // built layout are reused across frames when their inputs are unchanged, so a
    // pure scroll or selection change rebuilds nothing (see `prepaint`).
    text_version: u64,
    parsed: Option<Rc<ParsedDoc>>,
    parsed_version: u64,
    layout_sig: Option<LayoutSig>,
    shape_cache: HashMap<ShapeKey, WrappedLine>, // persistent shaped lines
    table_menu: Option<TableMenu>, // open table cell context menu, if any
}

/// An open table context menu: the window-space point to anchor it at and which
/// cell (source line + column) it acts on.
#[derive(Clone, Copy)]
struct TableMenu {
    pos: Point<Pixels>,
    line: usize,
    col: usize,
}

/// The context-menu items, in display order. The hit-test in the mouse handler
/// and the visual rows in `render` both index this, so they can't drift.
const TABLE_MENU_ITEMS: [(&str, TableOp); 6] = [
    ("Insert row above", TableOp::RowAbove),
    ("Insert row below", TableOp::RowBelow),
    ("Insert column left", TableOp::ColLeft),
    ("Insert column right", TableOp::ColRight),
    ("Delete row", TableOp::DeleteRow),
    ("Delete column", TableOp::DeleteColumn),
];
const TABLE_MENU_W: f32 = 190.0;
const TABLE_MENU_ITEM_H: f32 = 30.0;
const TABLE_MENU_VPAD: f32 = 6.0;

impl MarkdownEditor {
    pub fn new(
        content: impl Into<String>,
        base_dir: Option<PathBuf>,
        vault_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        ensure_fonts(cx);
        Self {
            text: content.into(),
            cursor: 0,
            anchor: None,
            dragging: false,
            scroll_top: px(0.),
            scroll_to_caret: true,
            layout: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_coalesce: None,
            focus_handle,
            base_dir,
            vault_dir,
            image_cache: HashMap::new(),
            vault_index: None,
            text_version: 0,
            parsed: None,
            parsed_version: 0,
            layout_sig: None,
            shape_cache: HashMap::new(),
            table_menu: None,
        }
    }

    /// Apply a table structural edit (from the context menu) and place the caret
    /// in the affected cell.
    fn apply_table_op(&mut self, line: usize, col: usize, op: TableOp, cx: &mut Context<Self>) {
        self.table_menu = None;
        let Some((new_text, fl, fc)) = edit_table(&self.text, line, col, op) else {
            cx.notify();
            return;
        };
        self.record(None);
        self.text = new_text;
        self.text_version = self.text_version.wrapping_add(1);
        // Place the caret at the focused cell's content start.
        let ls = line_start(&self.text, fl);
        let spans = table_cell_spans(line_str(&self.text, fl));
        self.cursor = spans.get(fc).map(|r| ls + r.start).unwrap_or(ls);
        self.anchor = None;
        self.last_coalesce = None;
        self.scroll_to_caret = true;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Parsed markdown for the current buffer, memoized until the text changes.
    /// Spans and markers are pre-bucketed per source line so building a line is
    /// O(line length) rather than O(total spans).
    fn ensure_parsed(&mut self) -> Rc<ParsedDoc> {
        if let Some(p) = &self.parsed {
            if self.parsed_version == self.text_version {
                return p.clone();
            }
        }
        let (spans, markers, links) = parse(&self.text);
        let code = classify_code(&self.text);
        let code_hl = highlight_code(&self.text, &code);
        let fm_end = frontmatter_end(&self.text);

        let n = code.len().max(1); // classify_code yields one entry per source line
        let mut line_spans: Vec<Vec<Span>> = vec![Vec::new(); n];
        let mut line_markers: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        let mut off = 0usize;
        for (li, line) in self.text.split('\n').enumerate() {
            let (ls, le) = (off, off + line.len());
            for s in &spans {
                if s.start < le && s.end > ls {
                    line_spans[li].push(s.clone());
                }
            }
            for &m in &markers {
                if m.1 > ls && m.0 < le {
                    line_markers[li].push(m);
                }
            }
            off = le + 1; // + newline
        }

        let doc = Rc::new(ParsedDoc {
            links,
            code,
            fm_end,
            line_spans,
            line_markers,
            code_hl,
        });
        self.parsed = Some(doc.clone());
        self.parsed_version = self.text_version;
        doc
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Place the caret at a byte offset (clamped to a char boundary). Handy for
    /// tests/demos that want to open with the caret somewhere specific.
    pub fn place_cursor(&mut self, offset: usize) {
        let mut o = offset.min(self.text.len());
        while o > 0 && !self.text.is_char_boundary(o) {
            o -= 1;
        }
        self.cursor = o;
        self.anchor = None;
    }

    pub fn set_text(
        &mut self,
        content: impl Into<String>,
        base_dir: Option<PathBuf>,
        vault_dir: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.text = content.into();
        self.cursor = 0;
        self.anchor = None;
        self.scroll_top = px(0.);
        self.scroll_to_caret = true;
        self.layout = None;
        self.layout_sig = None;
        self.shape_cache.clear();
        self.text_version = self.text_version.wrapping_add(1);
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_coalesce = None;
        self.base_dir = base_dir;
        self.vault_dir = vault_dir;
        self.image_cache.clear();
        self.vault_index = None;
        cx.notify();
    }

    /// Resolve and decode an image reference, caching the result (including
    /// failures, so a bad path isn't retried every frame).
    fn image_for(&mut self, r: &ImageRef) -> Option<Arc<RenderImage>> {
        let path = match r {
            ImageRef::Rel(p) => self.base_dir.as_ref()?.join(p),
            ImageRef::Embed(name) => self.resolve_embed(name)?,
        };
        self.image_cache
            .entry(path.clone())
            .or_insert_with(|| load_image(&path))
            .clone()
    }

    /// Resolve an Obsidian `![[name]]` embed: try note-dir and vault-root direct
    /// hits, then fall back to a vault-wide basename index (like Obsidian).
    fn resolve_embed(&mut self, name: &str) -> Option<PathBuf> {
        for base in [self.base_dir.as_deref(), self.vault_dir.as_deref()]
            .into_iter()
            .flatten()
        {
            let direct = base.join(name);
            if direct.is_file() {
                return Some(direct);
            }
        }
        if self.vault_index.is_none() {
            self.vault_index = Some(build_vault_index(self.vault_dir.as_deref()));
        }
        let key = name.rsplit(['/', '\\']).next().unwrap_or(name).to_lowercase();
        self.vault_index.as_ref().unwrap().get(&key).cloned()
    }

    fn selection(&self) -> Option<Range<usize>> {
        match self.anchor {
            Some(a) if a != self.cursor => Some(a.min(self.cursor)..a.max(self.cursor)),
            _ => None,
        }
    }

    // ---- undo/redo ----

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.text = s.text;
        self.cursor = s.cursor;
        self.anchor = s.anchor;
        self.text_version = self.text_version.wrapping_add(1);
    }

    fn record(&mut self, coalesce: Option<u8>) {
        if let Some(k) = coalesce {
            if self.last_coalesce == Some(k) {
                return;
            }
        }
        self.undo_stack.push(self.snapshot());
        if self.undo_stack.len() > UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        self.last_coalesce = coalesce;
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(self.snapshot());
            self.restore(prev);
            self.last_coalesce = None;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.snapshot());
            self.restore(next);
            self.last_coalesce = None;
        }
    }

    // ---- editing ----

    fn delete_selection(&mut self) -> bool {
        if let Some(sel) = self.selection() {
            self.text.replace_range(sel.clone(), "");
            self.cursor = sel.start;
            self.anchor = None;
            self.text_version = self.text_version.wrapping_add(1);
            true
        } else {
            false
        }
    }

    fn insert(&mut self, s: &str) {
        let coalesce = (s.chars().count() == 1 && !s.contains(char::is_whitespace)).then_some(1);
        self.record(coalesce);
        self.delete_selection();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.anchor = None;
        self.text_version = self.text_version.wrapping_add(1);
    }

    fn backspace(&mut self) {
        if self.selection().is_none() && self.cursor == 0 {
            return;
        }
        self.record(Some(2));
        if self.delete_selection() {
            return;
        }
        let prev = prev_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.text_version = self.text_version.wrapping_add(1);
    }

    // ---- clipboard ----

    fn copy(&self, cx: &mut Context<Self>) {
        if let Some(sel) = self.selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text[sel].to_string()));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = self.selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text[sel.clone()].to_string()));
            self.record(None);
            self.text.replace_range(sel.clone(), "");
            self.cursor = sel.start;
            self.anchor = None;
            self.text_version = self.text_version.wrapping_add(1);
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(s) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if s.is_empty() {
            return;
        }
        self.record(None);
        self.delete_selection();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
        self.anchor = None;
        self.text_version = self.text_version.wrapping_add(1);
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
        self.last_coalesce = None;
    }

    fn set_cursor(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
        self.last_coalesce = None;
    }

    /// If `line_idx` is a table row (header/body, not the separator), return its
    /// document start, byte length, and the source ranges of its cells' content.
    fn table_row_cells(&self, line_idx: usize) -> Option<(usize, usize, Vec<Range<usize>>)> {
        let lines: Vec<&str> = self.text.split('\n').collect();
        for (s, e) in table_blocks(&lines) {
            if (s..=e).contains(&line_idx) {
                if line_idx == s + 1 {
                    return None; // the `---` separator row
                }
                let ls = line_start(&self.text, line_idx);
                return Some((ls, lines[line_idx].len(), table_cell_spans(lines[line_idx])));
            }
        }
        None
    }

    /// Snap an offset that landed in a table row's pipe/padding gap onto a cell
    /// content boundary — forward to the next cell (or line end), backward to the
    /// previous cell (or line start). Keeps the caret inside editable cells. A
    /// non-table offset, or one already inside a cell, is returned unchanged.
    fn snap_table_h(&self, offset: usize, forward: bool) -> usize {
        let (line, _) = offset_to_line_col(&self.text, offset);
        let Some((ls, len, spans)) = self.table_row_cells(line) else {
            return offset;
        };
        let local = offset.saturating_sub(ls);
        if spans.iter().any(|r| local >= r.start && local <= r.end) {
            return offset;
        }
        if forward {
            spans
                .iter()
                .find(|r| r.start >= local)
                .map(|r| ls + r.start)
                .unwrap_or(ls + len)
        } else {
            spans
                .iter()
                .rev()
                .find(|r| r.end <= local)
                .map(|r| ls + r.end)
                .unwrap_or(ls)
        }
    }

    /// Move up/down one visual row using the last painted layout (wrap-aware).
    fn move_vertical(&mut self, down: bool, shift: bool) {
        if let Some(layout) = self.layout.clone() {
            if let Some((x, y, lh)) = layout.caret(self.cursor) {
                let target_y = if down { y + lh + px(1.) } else { (y - px(1.)).max(px(0.)) };
                let new = layout.offset_at(x, target_y);
                self.set_cursor(new, shift);
                return;
            }
        }
        // Fallback: source-line movement (before first layout).
        let (line, col) = offset_to_line_col(&self.text, self.cursor);
        if down {
            if line + 1 < self.text.split('\n').count() {
                self.set_cursor(line_col_to_offset(&self.text, line + 1, col), shift);
            }
        } else if line > 0 {
            self.set_cursor(line_col_to_offset(&self.text, line - 1, col), shift);
        }
    }

    /// If a content-space point falls on a rendered checkbox glyph, return the
    /// document offset of its toggle char (the byte between the brackets).
    fn checkbox_hit(&self, x: Pixels, content_y: Pixels) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let l = layout.line_at_y(content_y)?;
        let local_y = content_y - l.top;
        for c in &l.checks {
            let p0 = l
                .wrapped
                .position_for_index(c.disp.start, l.line_height)
                .unwrap_or_default();
            let p1 = l
                .wrapped
                .position_for_index(c.disp.end, l.line_height)
                .unwrap_or(p0);
            if local_y >= p0.y
                && local_y < p0.y + l.line_height
                && x >= p0.x - px(3.)
                && x <= p1.x + px(4.)
            {
                return Some(c.inner_abs);
            }
        }
        None
    }

    fn toggle_checkbox(&mut self, inner_abs: usize) {
        if inner_abs >= self.text.len() {
            return;
        }
        let repl = if self.text.as_bytes()[inner_abs] == b' ' {
            "x"
        } else {
            " "
        };
        self.record(None);
        self.text.replace_range(inner_abs..inner_abs + 1, repl);
        self.last_coalesce = None;
        self.text_version = self.text_version.wrapping_add(1);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // Any key dismisses an open table menu (Escape does nothing else).
        if self.table_menu.is_some() {
            self.table_menu = None;
            cx.notify();
            if ks.key == "escape" {
                return;
            }
        }
        if ks.modifiers.platform || ks.modifiers.control {
            let edited = match ks.key.as_str() {
                "c" => {
                    self.copy(cx);
                    false
                }
                "x" => {
                    self.cut(cx);
                    true
                }
                "v" => {
                    self.paste(cx);
                    true
                }
                "a" => {
                    self.select_all();
                    false
                }
                "z" if ks.modifiers.shift => {
                    self.redo();
                    true
                }
                "z" => {
                    self.undo();
                    true
                }
                "y" => {
                    self.redo();
                    true
                }
                _ => return, // e.g. Ctrl+S falls through to the host
            };
            if edited {
                cx.emit(EditorEvent::Changed);
            }
            self.scroll_to_caret = true;
            cx.notify();
            return;
        }
        let shift = ks.modifiers.shift;
        let edited = match ks.key.as_str() {
            "backspace" => {
                self.backspace();
                true
            }
            "enter" | "return" => {
                self.insert("\n");
                true
            }
            "space" => {
                self.insert(" ");
                true
            }
            "tab" => {
                self.insert("  ");
                true
            }
            "left" => {
                if self.cursor > 0 {
                    let p = self.snap_table_h(prev_boundary(&self.text, self.cursor), false);
                    self.set_cursor(p, shift);
                }
                false
            }
            "right" => {
                if let Some(ch) = self.text[self.cursor..].chars().next() {
                    let p = self.snap_table_h(self.cursor + ch.len_utf8(), true);
                    self.set_cursor(p, shift);
                }
                false
            }
            "up" => {
                self.move_vertical(false, shift);
                false
            }
            "down" => {
                self.move_vertical(true, shift);
                false
            }
            "home" => {
                let (line, _) = offset_to_line_col(&self.text, self.cursor);
                // In a table row, Home goes to the first cell's content, not the `|`.
                let p = match self.table_row_cells(line) {
                    Some((ls, _, spans)) if !spans.is_empty() => ls + spans[0].start,
                    _ => line_start(&self.text, line),
                };
                self.set_cursor(p, shift);
                false
            }
            "end" => {
                let (line, _) = offset_to_line_col(&self.text, self.cursor);
                let p = match self.table_row_cells(line) {
                    Some((ls, _, spans)) if !spans.is_empty() => ls + spans.last().unwrap().end,
                    _ => line_start(&self.text, line) + line_str(&self.text, line).len(),
                };
                self.set_cursor(p, shift);
                false
            }
            _ => {
                if let Some(ch) = &ks.key_char {
                    self.insert(ch);
                    true
                } else {
                    false
                }
            }
        };
        if edited {
            cx.emit(EditorEvent::Changed);
        }
        self.scroll_to_caret = true;
        cx.notify();
    }
}

impl EventEmitter<EditorEvent> for MarkdownEditor {}

impl MarkdownEditor {
    /// The table cell context menu, anchored at the (window-space) click point.
    /// It's visual only — clicks are resolved by the element's mouse handler
    /// (which hit-tests the same window-space rectangle), so the two can't drift.
    fn render_table_menu(&self, m: TableMenu) -> impl IntoElement {
        let items = TABLE_MENU_ITEMS.iter().enumerate().map(|(i, (label, op))| {
            let danger = matches!(op, TableOp::DeleteRow | TableOp::DeleteColumn);
            div()
                .id(("table-menu-item", i))
                .h(px(TABLE_MENU_ITEM_H))
                .px_3()
                .flex()
                .items_center()
                .text_size(px(13.))
                .text_color(if danger { rgb(0xdd8a8a) } else { rgb(TEXT) })
                .hover(|s| s.bg(rgb(0x3a3a3a)))
                .child(SharedString::from(*label))
        });
        deferred(
            anchored().position(m.pos).child(
                div()
                    .w(px(TABLE_MENU_W))
                    .py(px(TABLE_MENU_VPAD))
                    .bg(rgb(0x262626))
                    .border_1()
                    .border_color(rgb(0x3a3a3a))
                    .rounded(px(6.))
                    .shadow_lg()
                    .children(items),
            ),
        )
    }
}

impl Render for MarkdownEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus_handle)
            .key_context("MarkdownEditor")
            .on_key_down(cx.listener(Self::on_key))
            .p_8()
            .child(EditorElement {
                editor: cx.entity(),
            })
            .children(self.table_menu.map(|m| self.render_table_menu(m)))
    }
}

// ---------------------------------------------------------------------------
// Layout: the visual (wrapped) lines, shared between paint and the entity.
// ---------------------------------------------------------------------------

struct VisualLine {
    start: usize,   // document byte offset of the line start
    source: String, // raw source of the line
    segs: Vec<Seg>, // source<->display transform (conceal + substitution)
    checks: Vec<CheckHit>, // clickable checkbox glyphs on this line
    code_bg: bool,      // paint a full-width code panel behind this line
    hr: bool,           // render a horizontal rule instead of text
    left_inset: Pixels, // whole-line indent (blockquotes)
    quote_bars: u8,     // number of blockquote bars to paint at the left
    table: Option<TableRender>, // rendered as a table row instead of text
    image: Option<(Arc<RenderImage>, Size<Pixels>)>, // rendered in place of text
    wrapped: WrappedLine,
    line_height: Pixels,
    top: Pixels,    // y offset from content top
    height: Pixels, // total height incl. wrapped rows
    hang: Pixels,   // hanging indent: wrapped rows shift right by this much
}

impl VisualLine {
    /// Total x-offset for a point at local y: the whole-line inset plus, for
    /// wrapped continuation rows of a list item, the hanging indent.
    fn x_offset(&self, local_y: Pixels) -> Pixels {
        let hang = if self.hang > px(0.) && local_y >= self.line_height {
            self.hang
        } else {
            px(0.)
        };
        self.left_inset + hang
    }
}

struct Layout {
    lines: Vec<VisualLine>,
    total_height: Pixels,
    viewport_height: Pixels,
    links: Links, // document source range -> link target
}

impl Layout {
    /// The link target whose source range contains `offset`, if any.
    fn link_at(&self, offset: usize) -> Option<LinkTarget> {
        self.links
            .iter()
            .find(|(r, _)| r.start <= offset && offset < r.end)
            .map(|(_, t)| t.clone())
    }
}

impl Layout {
    fn line_at_y(&self, content_y: Pixels) -> Option<&VisualLine> {
        self.lines
            .iter()
            .find(|l| content_y >= l.top && content_y < l.top + l.height)
            .or_else(|| self.lines.last())
    }

    /// Document offset at a content-space point.
    fn offset_at(&self, x: Pixels, content_y: Pixels) -> usize {
        let Some(l) = self.line_at_y(content_y) else {
            return 0;
        };
        // Table row: hit-test within the clicked cell and map to its source.
        if let Some(TableRender::Row(cells)) = &l.table {
            if let Some(cell) = pick_cell(cells, x) {
                let text_x = (x - cell.x - cell.text_x()).max(px(0.));
                let disp = cell.shaped.closest_index_for_x(text_x);
                return cell.src_start + disp_to_src(&cell.segs, disp);
            }
            return l.start;
        }
        let local_y = content_y - l.top;
        // Undo the hanging indent before hit-testing continuation rows.
        let local_x = (x - l.x_offset(local_y)).max(px(0.));
        let local = point(local_x, local_y);
        let disp = l
            .wrapped
            .closest_index_for_position(local, l.line_height)
            .unwrap_or_else(|e| e);
        l.start + disp_to_src(&l.segs, disp)
    }

    /// Content-space caret position (x, y-top, line_height) for a document offset.
    fn caret(&self, cursor: usize) -> Option<(Pixels, Pixels, Pixels)> {
        // Table row: place the caret inside the cell whose source contains it
        // (snapping into the nearest cell if the offset landed in a pipe gap).
        for l in &self.lines {
            let Some(TableRender::Row(cells)) = &l.table else { continue };
            let Some(cell) = cells
                .iter()
                .find(|c| cursor >= c.src_start && cursor <= c.src_end)
                .or_else(|| in_table_line(l, cursor).then(|| pick_cell(cells, px(0.))).flatten())
            else {
                continue;
            };
            let off = cursor.clamp(cell.src_start, cell.src_end);
            let disp = src_to_disp(&cell.segs, off - cell.src_start);
            let cx = cell.x + cell.text_x() + cell.shaped.x_for_index(disp);
            return Some((cx, l.top + px(TABLE_PAD_Y), px(TABLE_FONT * 1.5)));
        }
        let l = self
            .lines
            .iter()
            .find(|l| l.table.is_none() && cursor >= l.start && cursor <= l.start + l.source.len())?;
        let disp = src_to_disp(&l.segs, cursor - l.start);
        let p = l
            .wrapped
            .position_for_index(disp, l.line_height)
            .unwrap_or_default();
        Some((p.x + l.x_offset(p.y), l.top + p.y, l.line_height))
    }
}

impl Layout {
    /// The (document line-start offset, column index) of the table cell at a
    /// content-space point, if the point is on a rendered table row.
    fn table_cell_at(&self, x: Pixels, content_y: Pixels) -> Option<(usize, usize)> {
        let l = self.line_at_y(content_y)?;
        let TableRender::Row(cells) = l.table.as_ref()? else {
            return None;
        };
        let col = cells
            .iter()
            .position(|c| x >= c.x && x < c.x + c.width)
            .unwrap_or(if cells.is_empty() {
                0
            } else if x < cells[0].x {
                0
            } else {
                cells.len() - 1
            });
        Some((l.start, col))
    }
}

/// Whether `offset` falls on this (table) line's source range.
fn in_table_line(l: &VisualLine, offset: usize) -> bool {
    offset >= l.start && offset <= l.start + l.source.len()
}

/// The cell at content-space x (the one whose box contains x, else the nearest).
fn pick_cell(cells: &[TableCell], x: Pixels) -> Option<&TableCell> {
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .find(|c| x >= c.x && x < c.x + c.width)
        .or_else(|| if x < cells[0].x { cells.first() } else { cells.last() })
}

// ---------------------------------------------------------------------------
// The custom element: shapes + wraps lines, paints text/selection/caret with
// scrolling, and routes mouse + wheel events back to the editor entity.
// ---------------------------------------------------------------------------

struct EditorElement {
    editor: Entity<MarkdownEditor>,
}

/// Memoized parse of the whole buffer (see [`MarkdownEditor::ensure_parsed`]).
/// `line_spans`/`line_markers` bucket the global spans/markers by the source line
/// they touch, so per-line building doesn't scan every span.
struct ParsedDoc {
    links: Links,
    code: Vec<CodeLine>,
    fm_end: Option<usize>,
    line_spans: Vec<Vec<Span>>,
    line_markers: Vec<Vec<(usize, usize)>>,
    /// Syntax-highlight colors for fenced code-block lines (line index -> run
    /// list of `(byte_len, color)`). Computed once per edit, not per frame.
    code_hl: HashMap<usize, Vec<(usize, Hsla)>>,
}

/// The inputs a built [`Layout`] depends on. When these are unchanged between
/// frames (a pure scroll, or a selection change) the layout is reused as-is.
#[derive(Clone, Copy, PartialEq, Eq)]
struct LayoutSig {
    version: u64,   // text_version
    width: u32,     // wrap width, f32 bits
    cursor_line: usize,
    cursor_col: usize, // line-local; drives the caret-line reveal (e.g. checkbox)
    // Reveal is now per-span (a marker shows only when the caret/selection is
    // within its span), so the selection is part of the layout's identity too.
    selection: Option<(usize, usize)>,
}

/// Cache key for a shaped line. A `WrappedLine` is fully determined by its
/// display text, font size, wrap width and runs (the runs also bake in colors /
/// decorations, so those are folded into `runs`). Same key → identical shaping.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: SharedString,
    font_size: u32, // f32 bits
    width: u32,     // f32 bits (already clamped)
    runs: u64,      // digest of the TextRun list
}

/// A digest of a run list covering everything that affects the shaped output:
/// geometry (font, size implied by key, length) and decoration (color, bg,
/// underline/strikethrough presence).
fn runs_digest(runs: &[TextRun]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for r in runs {
        r.len.hash(&mut h);
        r.font.hash(&mut h);
        [r.color.h, r.color.s, r.color.l, r.color.a].map(f32::to_bits).hash(&mut h);
        match &r.background_color {
            Some(c) => {
                1u8.hash(&mut h);
                [c.h, c.s, c.l, c.a].map(f32::to_bits).hash(&mut h);
            }
            None => 0u8.hash(&mut h),
        }
        r.underline.is_some().hash(&mut h);
        r.strikethrough.is_some().hash(&mut h);
    }
    h.finish()
}

/// Shape one display line, reusing a previously shaped `WrappedLine` when the
/// same (text, size, width, runs) was shaped before. Hits move from `old` into
/// `new`; whatever stays in `old` at the end of a build is dropped, bounding the
/// cache to the lines actually used this frame.
#[allow(clippy::too_many_arguments)]
fn shape_cached(
    old: &mut HashMap<ShapeKey, WrappedLine>,
    new: &mut HashMap<ShapeKey, WrappedLine>,
    window: &mut Window,
    display: &SharedString,
    font_size: Pixels,
    runs: &[TextRun],
    width: Pixels,
) -> WrappedLine {
    let shape_width = width.max(px(40.));
    let key = ShapeKey {
        text: display.clone(),
        font_size: f32::from(font_size).to_bits(),
        width: f32::from(shape_width).to_bits(),
        runs: runs_digest(runs),
    };
    let wrapped = old
        .remove(&key)
        .or_else(|| new.get(&key).cloned())
        .unwrap_or_else(|| {
            window
                .text_system()
                .shape_text(display.clone(), font_size, runs, Some(shape_width), None)
                .expect("shape_text")
                .into_iter()
                .next()
                .expect("one wrapped line")
        });
    new.insert(key, wrapped.clone());
    wrapped
}

struct Prepaint {
    layout: Rc<Layout>,
    scroll_top: Pixels,
    cursor: usize,
    selection: Option<(usize, usize)>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl EditorElement {
    /// Finish a prepaint: clamp scroll (keeping the caret in view), persist it,
    /// and package the state for `paint`. Shared by the cache-hit and rebuild
    /// paths so both handle scrolling identically.
    fn finish(
        &mut self,
        layout: Rc<Layout>,
        mut scroll_top: Pixels,
        scroll_to_caret: bool,
        cursor: usize,
        selection: Option<(usize, usize)>,
        viewport_height: Pixels,
        cx: &mut App,
    ) -> Prepaint {
        if scroll_to_caret {
            if let Some((_, cy, lh)) = layout.caret(cursor) {
                if cy < scroll_top {
                    scroll_top = cy;
                } else if cy + lh > scroll_top + viewport_height {
                    scroll_top = cy + lh - viewport_height;
                }
            }
        }
        let max_scroll = (layout.total_height - viewport_height).max(px(0.));
        scroll_top = scroll_top.clamp(px(0.), max_scroll);
        self.editor.update(cx, |e, _| {
            e.scroll_top = scroll_top;
            e.scroll_to_caret = false;
        });
        Prepaint {
            layout,
            scroll_top,
            cursor,
            selection,
        }
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let (cursor, selection, scroll_top, scroll_to_caret, version) = {
            let e = self.editor.read(cx);
            (
                e.cursor,
                e.selection().map(|r| (r.start, r.end)),
                e.scroll_top,
                e.scroll_to_caret,
                e.text_version,
            )
        };
        let wrap_width = bounds.size.width.max(px(80.));
        let viewport_height = bounds.size.height;
        let (cursor_line, cursor_col) = {
            let e = self.editor.read(cx);
            offset_to_line_col(&e.text, cursor)
        };
        let sig = LayoutSig {
            version,
            width: f32::from(wrap_width).to_bits(),
            cursor_line,
            cursor_col,
            selection,
        };

        // Fast path: when the layout's inputs are unchanged (a pure scroll or a
        // selection change) reuse the last one, instead of re-parsing and
        // re-shaping every line. This is what keeps scrolling large notes smooth.
        let cached = self
            .editor
            .update(cx, |e, _| (e.layout_sig == Some(sig)).then(|| e.layout.clone()).flatten());
        if let Some(layout) = cached {
            return self.finish(layout, scroll_top, scroll_to_caret, cursor, selection, viewport_height, cx);
        }

        // Slow path: (re)build the layout from a memoized parse. Shaped lines are
        // served from a persistent cache (`shape_cache`) so a rebuild only shapes
        // the lines whose text/style actually changed — the rest are reused even
        // across the fast-path frames that would otherwise evict GPUI's own cache.
        let text = self.editor.read(cx).text.clone();
        let parsed = self.editor.update(cx, |e, _| e.ensure_parsed());
        let mut old_shapes = self.editor.update(cx, |e, _| std::mem::take(&mut e.shape_cache));
        let mut new_shapes: HashMap<ShapeKey, WrappedLine> = HashMap::new();
        let mut base_font = window.text_style().font();
        base_font.family = UI_FONT.into();
        let mut mono_font = base_font.clone();
        mono_font.family = MONO_FONT.into();
        let code = &parsed.code;
        let fm_end = parsed.fm_end;

        // Pre-shape rendered tables. Tables are ALWAYS rendered (never dumped as
        // raw markdown) — the caret is placed and edited directly inside the
        // rendered cells. We measure columns, then lay out each row's cells,
        // revealing inline markup only in the cell/span the caret touches.
        let lines_vec: Vec<&str> = text.split('\n').collect();
        let mut line_off = Vec::with_capacity(lines_vec.len());
        {
            let mut acc = 0usize;
            for l in &lines_vec {
                line_off.push(acc);
                acc += l.len() + 1;
            }
        }
        let mut table_map: HashMap<usize, TableRender> = HashMap::new();
        for (start, end) in table_blocks(&lines_vec) {
            let aligns = table_aligns(lines_vec[start + 1]);
            let row_lis: Vec<usize> = (start..=end).filter(|&li| li != start + 1).collect();
            // Build each row's cells (shaped + source map), tracking column widths.
            let mut rows: Vec<Vec<TableCell>> = Vec::new();
            let mut colw: Vec<Pixels> = Vec::new();
            for (ri, &li) in row_lis.iter().enumerate() {
                let weight = if ri == 0 { FontWeight::BOLD } else { FontWeight::NORMAL };
                let mut cells = Vec::new();
                for (c, span) in table_cell_spans(lines_vec[li]).into_iter().enumerate() {
                    let cds = line_off[li] + span.start;
                    let cell_text = &lines_vec[li][span.clone()];
                    let (display, runs, segs) =
                        build_cell(cell_text, cds, &base_font, weight, cursor, selection);
                    let sl = window.text_system().shape_line(
                        display.into(),
                        px(TABLE_FONT),
                        &runs,
                        None,
                    );
                    if c >= colw.len() {
                        colw.resize(c + 1, px(0.));
                    }
                    colw[c] = colw[c].max(sl.width);
                    cells.push(TableCell {
                        shaped: sl,
                        segs,
                        src_start: cds,
                        src_end: line_off[li] + span.end,
                        x: px(0.), // filled in once column widths are known
                        width: px(0.),
                        align: aligns.get(c).copied().unwrap_or(TableAlign::Left),
                    });
                }
                rows.push(cells);
            }
            for w in &mut colw {
                *w += px(TABLE_PAD_X * 2.);
            }
            let mut colx: Vec<Pixels> = vec![px(0.)];
            for w in &colw {
                colx.push(*colx.last().unwrap() + *w);
            }
            // Assign column geometry and register each row/separator line.
            let mut ri = 0;
            for li in start..=end {
                if li == start + 1 {
                    table_map.insert(li, TableRender::Sep);
                    continue;
                }
                let mut cells = std::mem::take(&mut rows[ri]);
                for (c, cell) in cells.iter_mut().enumerate() {
                    cell.x = colx[c];
                    cell.width = colw[c];
                }
                table_map.insert(li, TableRender::Row(cells));
                ri += 1;
            }
        }

        let mut lines = Vec::new();
        let mut offset = 0usize;
        let mut top = px(0.);
        for (li, line) in text.split('\n').enumerate() {
            let is_cursor_line = li == cursor_line;

            // A rendered table row/separator: painted as a grid, no text shaping.
            if let Some(tr) = table_map.remove(&li) {
                let lh = px(TABLE_FONT * 1.5);
                let (segs, height) = match &tr {
                    TableRender::Sep => (Vec::new(), px(0.)),
                    TableRender::Row(_) => (
                        vec![Seg { src: 0..line.len(), disp: 0..0, sub: true }],
                        lh + px(TABLE_PAD_Y * 2.),
                    ),
                };
                let wrapped = window
                    .text_system()
                    .shape_text(SharedString::from(" "), lh, &[], Some(wrap_width), None)
                    .expect("shape_text")
                    .into_iter()
                    .next()
                    .expect("one line");
                lines.push(VisualLine {
                    start: offset,
                    source: line.to_string(),
                    segs,
                    checks: Vec::new(),
                    code_bg: false,
                    hr: false,
                    left_inset: px(0.),
                    quote_bars: 0,
                    table: Some(tr),
                    image: None,
                    wrapped,
                    line_height: lh,
                    top,
                    height,
                    hang: px(0.),
                });
                top += height;
                offset += line.len() + 1;
                continue;
            }

            let in_block = code[li].in_block || fm_end.is_some_and(|e| li <= e);

            // A non-active image line renders the picture in place of its source.
            let image = if !is_cursor_line && !in_block {
                parse_image_line(line).and_then(|rel| {
                    self.editor.update(cx, |e, _| e.image_for(&rel))
                })
            } else {
                None
            };

            let (font_size, base_weight) = if in_block {
                (px(CODE_FONT), FontWeight::NORMAL)
            } else {
                heading_style(line)
            };
            let lh = line_height(font_size);
            let font = if in_block { &mono_font } else { &base_font };
            // Hide a ``` fence line unless the caret is inside that same block.
            let hide_fence = fence_hidden(code, cursor_line, li);
            // A horizontal rule (unless the caret is on it, so it stays editable).
            let hr = !is_cursor_line && !in_block && is_hr(line);
            // Blockquote: concealed `>` prefix + inset + left bar, unless editing.
            let quote = (!in_block && !hr && image.is_none()).then(|| quote_prefix(line)).flatten();
            let quoted = quote.filter(|_| !is_cursor_line);
            let (left_inset, quote_bars) = match quoted {
                Some((level, _)) => (px(QUOTE_INDENT * level as f32), level),
                None => (px(0.), 0),
            };
            // Caret column within this line (None if the caret is elsewhere).
            let cursor_col = is_cursor_line.then(|| cursor - offset);
            let ops = if image.is_some() || hide_fence || hr {
                vec![Op { src: 0..line.len(), repl: Repl::Hide }]
            } else if in_block {
                Vec::new()
            } else {
                let mut v = line_ops(
                    line,
                    offset,
                    &parsed.line_markers[li],
                    &parsed.line_spans[li],
                    cursor,
                    selection,
                );
                if let Some((_, plen)) = quoted {
                    v.insert(0, Op { src: 0..plen, repl: Repl::Hide });
                    v.sort_by_key(|o| o.src.start);
                }
                v
            };
            // A checkbox stays struck-through only while its glyph is rendered.
            let checked = checkbox_render(line, cursor_col)
                .map(|(_, checked, shown)| checked && shown)
                .unwrap_or(false);
            // Revealed markers (caret/selection touching their span) are dimmed
            // like Obsidian's gray markup; concealed ones aren't in the display.
            let dim = if in_block {
                Vec::new()
            } else {
                let mut d = dim_ranges(line, offset, &parsed.line_markers[li], &parsed.line_spans[li], cursor, selection);
                if is_cursor_line {
                    if let Some((_, plen)) = quote {
                        d.push((0, plen)); // dim the `>` on the line being edited
                    }
                }
                d
            };
            // Fenced-code content lines get syntax coloring instead of the
            // inline-markdown styling (which doesn't apply inside code anyway).
            let (display, runs, segs, checks) = if in_block && !code[li].is_fence {
                build_code_line(line, font, parsed.code_hl.get(&li).map_or(&[][..], |v| v))
            } else {
                build_line(line, offset, &ops, &parsed.line_spans[li], font, &dim, base_weight, checked)
            };
            let display = SharedString::from(display);
            let mut wrapped = shape_cached(
                &mut old_shapes,
                &mut new_shapes,
                window,
                &display,
                font_size,
                &runs,
                wrap_width,
            );

            // Hanging indent for wrapped list items: align continuation rows
            // under the text (after the bullet/checkbox), not the marker. Source
            // offset just past the `- `/`- [ ] ` prefix, mapped to display x.
            let hang_src_end = ops
                .iter()
                .find_map(|o| match &o.repl {
                    Repl::Bullet => Some(o.src.end + 1), // skip the space after the marker
                    Repl::Checkbox(_) => Some(o.src.end),
                    _ => None,
                })
                // Ordered lists keep their `1. ` visible but still hang-indent.
                .or_else(|| (!in_block).then(|| ordered_prefix_len(line)).flatten());
            let mut hang = px(0.);
            if let Some(se) = hang_src_end {
                let disp = src_to_disp(&segs, se.min(line.len()));
                hang = wrapped.position_for_index(disp, lh).map(|p| p.x).unwrap_or_default();
                // Re-shape narrower so shifted continuation rows don't overflow.
                if hang > px(0.) && wrapped.size(lh).height > lh {
                    wrapped = shape_cached(
                        &mut old_shapes,
                        &mut new_shapes,
                        window,
                        &display,
                        font_size,
                        &runs,
                        wrap_width - hang,
                    );
                }
            }

            // Fit an image to the editor width (never upscaling), capped in height.
            let image = image.map(|data| {
                let iw = data.size(0).width.0 as f32;
                let ih = data.size(0).height.0 as f32;
                let scale = (f32::from(wrap_width) / iw)
                    .min(1.0)
                    .min(IMAGE_MAX_H / ih.max(1.0));
                (data, size(px(iw * scale), px(ih * scale)))
            });
            let height = match &image {
                Some((_, sz)) => sz.height + px(IMAGE_PAD * 2.),
                None => wrapped.size(lh).height,
            };
            lines.push(VisualLine {
                start: offset,
                source: line.to_string(),
                segs,
                checks,
                code_bg: in_block,
                hr,
                left_inset,
                quote_bars,
                table: None,
                image,
                wrapped,
                line_height: lh,
                top,
                height,
                hang,
            });
            top += height;
            offset += line.len() + 1; // + newline
        }

        let total_height = top;
        let layout = Rc::new(Layout {
            lines,
            total_height,
            viewport_height,
            links: parsed.links.clone(),
        });
        self.editor.update(cx, |e, _| {
            e.layout = Some(layout.clone());
            e.layout_sig = Some(sig);
            e.shape_cache = new_shapes;
        });
        self.finish(layout, scroll_top, scroll_to_caret, cursor, selection, viewport_height, cx)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let st = prepaint.scroll_top;
        let top_edge = bounds.origin.y;
        let bottom_edge = bounds.origin.y + bounds.size.height;

        for line in &prepaint.layout.lines {
            let screen_top = bounds.origin.y - st + line.top;
            if screen_top + line.height < top_edge || screen_top > bottom_edge {
                continue;
            }

            // Code-block panel: a contiguous full-width shaded band.
            if line.code_bg {
                let rect = Bounds {
                    origin: point(bounds.origin.x - px(8.), screen_top),
                    size: size(bounds.size.width + px(16.), line.height),
                };
                window.paint_quad(fill(rect, rgb(CODE_PANEL)));
            }

            // Blockquote bars, one per nesting level, in the left inset.
            for k in 0..line.quote_bars {
                let bx = bounds.origin.x + px(QUOTE_INDENT * k as f32) + px(3.);
                let rect = Bounds {
                    origin: point(bx, screen_top),
                    size: size(px(3.), line.height),
                };
                window.paint_quad(fill(rect, rgb(MUTED)));
            }

            // Selection background (per wrapped row). Painted *after* the line's
            // run backgrounds (see below) so it shows over inline-code/highlight
            // backgrounds, which are opaque; hence a closure invoked at the right
            // point in each text-paint path rather than here.
            let paint_selection = |win: &mut Window| {
                let Some((ss, se)) = prepaint.selection else { return };
                let (ls, le) = (line.start, line.start + line.source.len());
                if se > ls && ss < le {
                    let a = src_to_disp(&line.segs, ss.max(ls) - ls);
                    let b = src_to_disp(&line.segs, se.min(le) - ls);
                    let lh = line.line_height;
                    let pa = line.wrapped.position_for_index(a, lh).unwrap_or_default();
                    let pb = line.wrapped.position_for_index(b, lh).unwrap_or_default();
                    let full_w = line.wrapped.size(lh).width;
                    let paint_row = |win: &mut Window, x0: Pixels, x1: Pixels, row_y: Pixels| {
                        let shift = line.x_offset(row_y);
                        let rect = Bounds {
                            origin: point(bounds.origin.x + x0 + shift, screen_top + row_y),
                            size: size((x1 - x0).max(px(3.)), lh),
                        };
                        win.paint_quad(fill(rect, rgb(SELECTION)));
                    };
                    if pa.y == pb.y {
                        paint_row(win, pa.x, pb.x, pa.y);
                    } else {
                        paint_row(win, pa.x, full_w, pa.y);
                        let mut ry = pa.y + lh;
                        while ry + px(0.5) < pb.y {
                            paint_row(win, px(0.), full_w, ry);
                            ry += lh;
                        }
                        paint_row(win, px(0.), pb.x, pb.y);
                    }
                }
            };

            if let Some((data, sz)) = &line.image {
                let rect = Bounds {
                    origin: point(bounds.origin.x, screen_top + px(IMAGE_PAD)),
                    size: *sz,
                };
                let _ = window.paint_image(rect, Corners::all(px(4.)), data.clone(), 0, false);
                continue;
            }

            // Horizontal rule: a thin centered line across the content width.
            if line.hr {
                paint_selection(window);
                let rect = Bounds {
                    origin: point(bounds.origin.x, screen_top + line.height / 2. - px(1.)),
                    size: size(bounds.size.width, px(2.)),
                };
                window.paint_quad(fill(rect, rgb(MUTED)));
                continue;
            }

            // Table row: each cell as a bordered, padded, aligned box.
            if let Some(TableRender::Row(cells)) = &line.table {
                for cell in cells {
                    let cx0 = bounds.origin.x + cell.x;
                    let rect = Bounds {
                        origin: point(cx0, screen_top),
                        size: size(cell.width, line.height),
                    };
                    window.paint_quad(quad(
                        rect,
                        px(0.),
                        transparent_black(),
                        px(1.),
                        rgb(TABLE_BORDER),
                        BorderStyle::Solid,
                    ));
                    let text_origin = point(cx0 + cell.text_x(), screen_top + px(TABLE_PAD_Y));
                    // paint_background draws inline code/highlight run backgrounds
                    // (ShapedLine::paint only draws glyphs).
                    let _ = cell.shaped.paint_background(text_origin, line.line_height, window, cx);
                    let _ = cell.shaped.paint(text_origin, line.line_height, window, cx);
                }
                continue;
            }
            if line.table.is_some() {
                continue; // separator row: nothing to paint
            }

            let lh = line.line_height;
            let base_x = bounds.origin.x + line.left_inset;
            if line.hang > px(0.) && line.height > lh {
                // Hanging indent: paint row 0 at the margin, continuation rows
                // shifted right, using vertical content masks to split them.
                let fx = bounds.origin.x - px(16.);
                let fw = bounds.size.width + px(32.);
                let mask0 = ContentMask {
                    bounds: Bounds { origin: point(fx, screen_top), size: size(fw, lh) },
                };
                window.with_content_mask(Some(mask0), |w| {
                    let o = point(base_x, screen_top);
                    let _ = line.wrapped.paint_background(o, lh, TextAlign::Left, None, w, cx);
                    paint_selection(w);
                    let _ = line.wrapped.paint(o, lh, TextAlign::Left, None, w, cx);
                });
                let mask1 = ContentMask {
                    bounds: Bounds {
                        origin: point(fx, screen_top + lh),
                        size: size(fw, line.height - lh),
                    },
                };
                window.with_content_mask(Some(mask1), |w| {
                    let o = point(base_x + line.hang, screen_top);
                    let _ = line.wrapped.paint_background(o, lh, TextAlign::Left, None, w, cx);
                    paint_selection(w);
                    let _ = line.wrapped.paint(o, lh, TextAlign::Left, None, w, cx);
                });
            } else {
                let o = point(base_x, screen_top);
                let _ = line.wrapped.paint_background(o, lh, TextAlign::Left, None, window, cx);
                paint_selection(window);
                let _ = line.wrapped.paint(o, lh, TextAlign::Left, None, window, cx);
            }

            // Checkboxes: painted over their reserved slot (always on row 0).
            for chk in &line.checks {
                let p = line
                    .wrapped
                    .position_for_index(chk.disp.start, lh)
                    .unwrap_or_default();
                let side = lh * 0.62;
                let pad = (lh - side) / 2.;
                let rect = Bounds {
                    origin: point(bounds.origin.x + p.x + px(1.), screen_top + pad),
                    size: size(side, side),
                };
                let radii = Corners::all(px(3.));
                if chk.checked {
                    window.paint_quad(quad(
                        rect,
                        radii,
                        rgb(ACCENT),
                        px(0.),
                        rgb(ACCENT),
                        BorderStyle::Solid,
                    ));
                    // White checkmark (Material Design tick outline), filled.
                    let o = rect.origin;
                    let pt = |fx: f32, fy: f32| point(o.x + side * fx, o.y + side * fy);
                    const TICK: [(f32, f32); 6] = [
                        (0.375, 0.674),
                        (0.201, 0.500),
                        (0.142, 0.559),
                        (0.375, 0.792),
                        (0.875, 0.292),
                        (0.816, 0.233),
                    ];
                    let mut path = gpui::Path::new(pt(TICK[0].0, TICK[0].1));
                    for &(fx, fy) in &TICK[1..] {
                        path.line_to(pt(fx, fy));
                    }
                    path.line_to(pt(TICK[0].0, TICK[0].1));
                    window.paint_path(path, rgb(0xffffff));
                } else {
                    window.paint_quad(quad(
                        rect,
                        radii,
                        transparent_black(),
                        px(1.5),
                        rgb(MUTED),
                        BorderStyle::Solid,
                    ));
                }
            }
        }

        // Caret.
        if let Some((x, cy, lh)) = prepaint.layout.caret(prepaint.cursor) {
            let sy = bounds.origin.y - st + cy;
            let caret = Bounds {
                origin: point(bounds.origin.x + x, sy + px(1.)),
                size: size(px(2.), lh - px(2.)),
            };
            window.paint_quad(fill(caret, rgb(CARET)));
        }

        // Mouse: click-to-place, drag-to-select.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Left
                    || !bounds.contains(&ev.position)
                {
                    return;
                }
                editor.update(cx, |e, cx| {
                    // A click while the table menu is open resolves against the
                    // menu (in window space, matching the anchored overlay), then
                    // closes it — it never falls through to move the caret.
                    if let Some(menu) = e.table_menu {
                        let rel = ev.position - menu.pos;
                        let h = TABLE_MENU_VPAD * 2. + TABLE_MENU_ITEMS.len() as f32 * TABLE_MENU_ITEM_H;
                        let inside = f32::from(rel.x) >= 0.
                            && f32::from(rel.x) <= TABLE_MENU_W
                            && f32::from(rel.y) >= 0.
                            && f32::from(rel.y) <= h;
                        e.table_menu = None;
                        if inside {
                            let idx = ((f32::from(rel.y) - TABLE_MENU_VPAD) / TABLE_MENU_ITEM_H) as usize;
                            if let Some((_, op)) = TABLE_MENU_ITEMS.get(idx) {
                                e.apply_table_op(menu.line, menu.col, *op, cx);
                            }
                        }
                        cx.notify();
                        return;
                    }
                    let Some(layout) = e.layout.clone() else { return };
                    let x = ev.position.x - bounds.origin.x;
                    let content_y = ev.position.y - bounds.origin.y + e.scroll_top;
                    e.focus_handle.focus(window);
                    // Double-click selects the word under the cursor; triple-click
                    // (or more) selects the whole line.
                    if !ev.modifiers.shift && ev.click_count >= 2 {
                        let offset = layout.offset_at(x, content_y);
                        let range = if ev.click_count == 2 {
                            word_at(&e.text, offset)
                        } else {
                            let line = offset_to_line_col(&e.text, offset).0;
                            let s = line_start(&e.text, line);
                            s..s + line_str(&e.text, line).len()
                        };
                        e.anchor = Some(range.start);
                        e.cursor = range.end;
                        e.last_coalesce = None;
                        e.dragging = false;
                        e.scroll_to_caret = false;
                        cx.notify();
                        return;
                    }
                    // A click on a rendered checkbox toggles it instead of moving the caret.
                    if !ev.modifiers.shift {
                        if let Some(inner) = e.checkbox_hit(x, content_y) {
                            e.toggle_checkbox(inner);
                            cx.emit(EditorEvent::Changed);
                            e.scroll_to_caret = false;
                            cx.notify();
                            return;
                        }
                    }
                    let offset = layout.offset_at(x, content_y);
                    // Click a rendered link to open it (unless its line already
                    // shows raw source, i.e. the caret is on it — then edit).
                    if !ev.modifiers.shift {
                        let clicked_line = offset_to_line_col(&e.text, offset).0;
                        let caret_line = offset_to_line_col(&e.text, e.cursor).0;
                        if clicked_line != caret_line {
                            match layout.link_at(offset) {
                                Some(LinkTarget::Url(url)) => {
                                    cx.open_url(&url);
                                    return;
                                }
                                Some(LinkTarget::Note(name)) => {
                                    cx.emit(EditorEvent::OpenNote(name.to_string()));
                                    return;
                                }
                                None => {}
                            }
                        }
                    }
                    if ev.modifiers.shift {
                        e.set_cursor(offset, true);
                    } else {
                        e.cursor = offset;
                        e.anchor = Some(offset);
                    }
                    e.last_coalesce = None;
                    e.dragging = true;
                    cx.notify();
                });
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || ev.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                editor.update(cx, |e, cx| {
                    if !e.dragging {
                        return;
                    }
                    let Some(layout) = e.layout.clone() else { return };
                    let content_y = ev.position.y - bounds.origin.y + e.scroll_top;
                    e.cursor = layout.offset_at(ev.position.x - bounds.origin.x, content_y);
                    cx.notify();
                });
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                    return;
                }
                editor.update(cx, |e, _| e.dragging = false);
            }
        });
        // Right-click on a table cell opens its add/delete row/column menu.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Right
                    || !bounds.contains(&ev.position)
                {
                    return;
                }
                editor.update(cx, |e, cx| {
                    let Some(layout) = e.layout.clone() else { return };
                    let x = ev.position.x - bounds.origin.x;
                    let content_y = ev.position.y - bounds.origin.y + e.scroll_top;
                    if let Some((line_start_off, col)) = layout.table_cell_at(x, content_y) {
                        e.focus_handle.focus(window);
                        let line = offset_to_line_col(&e.text, line_start_off).0;
                        // Anchor the menu at the (window-space) click point.
                        e.table_menu = Some(TableMenu { pos: ev.position, line, col });
                        cx.notify();
                    } else {
                        e.table_menu = None;
                    }
                });
            }
        });
        // Scroll wheel.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &ScrollWheelEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || !bounds.contains(&ev.position) {
                    return;
                }
                editor.update(cx, |e, cx| {
                    e.table_menu = None; // a transient overlay; close it on scroll
                    let dy = match ev.delta {
                        ScrollDelta::Pixels(p) => p.y,
                        ScrollDelta::Lines(l) => l.y * px(40.),
                    };
                    let max = e
                        .layout
                        .as_ref()
                        .map(|l| (l.total_height - l.viewport_height).max(px(0.)))
                        .unwrap_or(px(0.));
                    e.scroll_top = (e.scroll_top - dy).clamp(px(0.), max);
                    e.scroll_to_caret = false;
                    cx.notify();
                });
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Markdown parsing / styling / concealment.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Attr {
    Bold,
    Italic,
    Strike,
    Code,
    Link,
    Highlight,
    Tag,
}

#[derive(Clone)]
struct Span {
    start: usize,
    end: usize,
    attr: Attr,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct StyleKey {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
    highlight: bool,
    tag: bool,
}

/// A source→display transform for a source range within one line. `Copy` maps
/// 1:1 (styled text); `sub` segments (concealed markers, bullet/checkbox glyphs)
/// collapse or replace their source range with fixed display text.
#[derive(Clone)]
struct Seg {
    src: Range<usize>,  // line-local source bytes
    disp: Range<usize>, // display bytes
    sub: bool,          // non-1:1 (hidden or substituted)
}

/// One line-local edit to the display: hide the source range, replace it with a
/// bullet glyph, or reserve a slot for a painted checkbox.
enum Repl {
    Hide,
    Bullet,
    Checkbox(bool), // checked?
}

struct Op {
    src: Range<usize>, // line-local
    repl: Repl,
}

/// A painted checkbox: where to draw it (display range on row 0) and the
/// document offset of the toggle char, for click-to-toggle.
#[derive(Clone)]
struct CheckHit {
    inner_abs: usize,   // document offset of the char between the brackets
    disp: Range<usize>, // display range of the reserved box slot
    checked: bool,
}

// The bullet dot and the reserved checkbox slot (an em-space wide enough for the
// painted box, plus a trailing gap before the text).
const BULLET: &str = "\u{2022}";
const CHECKBOX_SLOT: &str = "\u{2003} "; // EM SPACE + space

const TABLE_PAD_X: f32 = 10.0; // horizontal cell padding (px)
const TABLE_PAD_Y: f32 = 5.0; // vertical cell padding (px)
const TABLE_FONT: f32 = 16.0; // table cell font size (px)
const TABLE_BORDER: u32 = 0x3a3a3a; // cell border color

#[derive(Clone, Copy, PartialEq)]
enum TableAlign {
    Left,
    Center,
    Right,
}

/// A rendered table cell: its shaped text plus column geometry (x/width are
/// relative to the table's left edge) and the source mapping needed to place the
/// caret / hit-test clicks inside it for in-cell editing.
struct TableCell {
    shaped: gpui::ShapedLine,
    segs: Vec<Seg>,     // cell-local source <-> display transform
    src_start: usize,   // document offset of the cell's content start
    src_end: usize,     // document offset of the cell's content end
    x: Pixels,
    width: Pixels,
    align: TableAlign,
}

impl TableCell {
    /// Horizontal offset of the shaped text within the cell box (alignment pad).
    fn text_x(&self) -> Pixels {
        let tw = self.shaped.width;
        match self.align {
            TableAlign::Left => px(TABLE_PAD_X),
            TableAlign::Right => (self.width - px(TABLE_PAD_X) - tw).max(px(TABLE_PAD_X)),
            TableAlign::Center => ((self.width - tw) / 2.).max(px(TABLE_PAD_X)),
        }
    }
}

/// How a source line participates in a rendered table.
enum TableRender {
    Sep,             // the `| --- |` separator row: draws nothing (0 height)
    Row(Vec<TableCell>),
}

struct Checkbox {
    bracket: Range<usize>, // line-local `[ ]` range
    checked: bool,
}

struct ListInfo {
    marker: Range<usize>,        // line-local list-marker char range (`-`/`*`/`+`)
    checkbox: Option<Checkbox>,
}

/// One open inline element while walking the parser.
struct Frame {
    attr: Attr,
    cstart: usize,          // container start (incl. markers)
    istart: Option<usize>,  // inner content start
    iend: usize,            // inner content end
    url: Option<SharedString>, // set for links
}

type Links = Vec<(Range<usize>, LinkTarget)>;

fn parse(text: &str) -> (Vec<Span>, Vec<(usize, usize)>, Links) {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    let mut spans = Vec::new();
    let mut markers = Vec::new();
    let mut links: Links = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    let push = |stack: &mut Vec<Frame>, attr, start, url| {
        stack.push(Frame {
            attr,
            cstart: start,
            istart: None,
            iend: start,
            url,
        });
    };

    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::Strong) => push(&mut stack, Attr::Bold, range.start, None),
            Event::Start(Tag::Emphasis) => push(&mut stack, Attr::Italic, range.start, None),
            Event::Start(Tag::Strikethrough) => push(&mut stack, Attr::Strike, range.start, None),
            Event::Start(Tag::Link { dest_url, .. }) => {
                push(&mut stack, Attr::Link, range.start, Some(dest_url.to_string().into()))
            }
            Event::End(TagEnd::Strong)
            | Event::End(TagEnd::Emphasis)
            | Event::End(TagEnd::Strikethrough)
            | Event::End(TagEnd::Link) => {
                if let Some(f) = stack.pop() {
                    let cend = range.end;
                    let istart = f.istart.unwrap_or(cend);
                    let iend = f.iend.max(istart);
                    if istart > f.cstart {
                        markers.push((f.cstart, istart));
                    }
                    if cend > iend {
                        markers.push((iend, cend));
                    }
                    spans.push(Span {
                        start: f.cstart,
                        end: cend,
                        attr: f.attr,
                    });
                    if let Some(url) = f.url {
                        links.push((f.cstart..cend, LinkTarget::Url(url)));
                    }
                    if let Some(parent) = stack.last_mut() {
                        if parent.istart.is_none() {
                            parent.istart = Some(f.cstart);
                        }
                        parent.iend = parent.iend.max(cend);
                    }
                }
            }
            Event::Text(_) => {
                if let Some(top) = stack.last_mut() {
                    if top.istart.is_none() {
                        top.istart = Some(range.start);
                    }
                    top.iend = top.iend.max(range.end);
                }
            }
            Event::Code(_) => {
                spans.push(Span {
                    start: range.start,
                    end: range.end,
                    attr: Attr::Code,
                });
                if range.end >= range.start + 2 {
                    markers.push((range.start, range.start + 1));
                    markers.push((range.end - 1, range.end));
                }
                if let Some(top) = stack.last_mut() {
                    if top.istart.is_none() {
                        top.istart = Some(range.start);
                    }
                    top.iend = top.iend.max(range.end);
                }
            }
            _ => {}
        }
    }

    scan_wikilinks(text, &mut spans, &mut markers, &mut links);
    scan_highlights(text, &mut spans, &mut markers);
    scan_tags(text, &mut spans);
    (spans, markers, links)
}

/// Obsidian `==highlight==`: conceal the `==` and give the text a highlight bg.
fn scan_highlights(text: &str, spans: &mut Vec<Span>, markers: &mut Vec<(usize, usize)>) {
    let mut i = 0;
    while let Some(rel) = text[i..].find("==") {
        let s = i + rel;
        let Some(rel_end) = text[s + 2..].find("==") else { break };
        let e = s + 2 + rel_end + 2;
        if text[s + 2..e - 2].trim().is_empty() || text[s + 2..e - 2].contains('\n') {
            i = s + 2;
            continue;
        }
        markers.push((s, s + 2));
        markers.push((e - 2, e));
        spans.push(Span { start: s, end: e, attr: Attr::Highlight });
        i = e;
    }
}

/// Inline `#tag`s (letters/digits/`_-/`, starting with a letter, at a word
/// boundary). Styled like a pill; not concealed. Headings (`# `) don't match.
fn scan_tags(text: &str, spans: &mut Vec<Span>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || b[i - 1].is_ascii_whitespace() || b[i - 1] == b'(';
        if b[i] == b'#' && boundary && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'-' | b'/')) {
                j += 1;
            }
            spans.push(Span { start: i, end: j, attr: Attr::Tag });
            i = j;
        } else {
            i += 1;
        }
    }
}

/// Obsidian `[[target]]` / `[[target|alias]]` links (pulldown doesn't know
/// them). Conceal the brackets (and `target|`), show the alias-or-target as a
/// styled internal link, and record the note name for click-to-open.
fn scan_wikilinks(text: &str, spans: &mut Vec<Span>, markers: &mut Vec<(usize, usize)>, links: &mut Links) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find("[[") {
        let s = i + rel;
        if s > 0 && bytes[s - 1] == b'!' {
            i = s + 2; // an ![[embed]] — handled at the line level
            continue;
        }
        let Some(rel_end) = text[s + 2..].find("]]") else { break };
        let e = s + 2 + rel_end + 2;
        let content = &text[s + 2..e - 2];
        if content.is_empty() || content.contains('\n') {
            i = s + 2;
            continue;
        }
        // Split off an optional `|alias`; the shown text is the alias or target.
        let (target, disp_start) = match content.find('|') {
            Some(p) => (&content[..p], s + 2 + p + 1),
            None => (content, s + 2),
        };
        markers.push((s, disp_start));
        markers.push((e - 2, e));
        spans.push(Span { start: s, end: e, attr: Attr::Link });
        let note = target.split('#').next().unwrap_or(target).trim();
        links.push((s..e, LinkTarget::Note(note.to_string().into())));
        i = e;
    }
}

fn heading_style(line: &str) -> (Pixels, FontWeight) {
    let t = line.trim_start();
    let hashes = t.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && t.as_bytes().get(hashes) == Some(&b' ') {
        // Heading sizes, scaled off the 18px body.
        let (size, weight) = match hashes {
            1 => (30., FontWeight::BOLD),
            2 => (25., FontWeight::BOLD),
            3 => (21., FontWeight::SEMIBOLD),
            4 => (20., FontWeight::SEMIBOLD),
            5 => (19., FontWeight::SEMIBOLD),
            _ => (18., FontWeight::SEMIBOLD),
        };
        (px(size), weight)
    } else {
        (px(BODY_FONT), FontWeight::NORMAL)
    }
}

fn style_at(pos: usize, spans: &[Span]) -> StyleKey {
    let mut k = StyleKey::default();
    for s in spans {
        if s.start <= pos && pos < s.end {
            match s.attr {
                Attr::Bold => k.bold = true,
                Attr::Italic => k.italic = true,
                Attr::Strike => k.strike = true,
                Attr::Code => k.code = true,
                Attr::Link => k.link = true,
                Attr::Highlight => k.highlight = true,
                Attr::Tag => k.tag = true,
            }
        }
    }
    k
}

fn text_run(key: StyleKey, len: usize, base_font: &Font, base_weight: FontWeight) -> TextRun {
    let mut font = base_font.clone();
    font.weight = if key.bold { FontWeight::BOLD } else { base_weight };
    if key.italic {
        font.style = FontStyle::Italic;
    }
    let color = if key.highlight {
        rgb(HIGHLIGHT_FG)
    } else if key.link || key.tag {
        rgb(LINK)
    } else {
        rgb(TEXT)
    };
    let background_color = if key.highlight {
        Some(rgb(HIGHLIGHT_BG).into())
    } else if key.tag {
        Some(rgba(TAG_BG).into())
    } else if key.code {
        Some(rgb(CODE_BG).into())
    } else {
        None
    };
    TextRun {
        len,
        font,
        color: color.into(),
        background_color,
        underline: key.link.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(rgb(LINK).into()),
            wavy: false,
        }),
        strikethrough: key.strike.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
    }
}

/// A blockquote prefix: leading `>`s (nested), returning the nesting level and
/// the byte length of the whole `> `/`> > ` prefix.
fn quote_prefix(line: &str) -> Option<(u8, usize)> {
    let mut i = line.len() - line.trim_start().len(); // skip any leading indent
    let mut level = 0u8;
    while line[i..].starts_with('>') {
        level += 1;
        i += 1;
        if line[i..].starts_with(' ') {
            i += 1;
        }
    }
    (level > 0).then_some((level, i))
}

/// A thematic break: a line of only `-`, `*` or `_` (3+), rendered as a rule.
fn is_hr(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.bytes().all(|b| b == b'-')
            || t.bytes().all(|b| b == b'*')
            || t.bytes().all(|b| b == b'_'))
}

/// Length of an ordered-list prefix (`12. ` or `12) `), for hanging indent.
fn ordered_prefix_len(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let after = &rest[digits..];
    if after.starts_with(". ") || after.starts_with(") ") {
        Some(indent + digits + 2)
    } else {
        None
    }
}

fn heading_prefix_len(line: &str) -> Option<usize> {
    let lead_ws = line.len() - line.trim_start().len();
    let rest = &line[lead_ws..];
    let hashes = rest.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let after = &rest[hashes..];
        let spaces = after.len() - after.trim_start_matches(' ').len();
        if spaces >= 1 {
            return Some(lead_ws + hashes + spaces);
        }
    }
    None
}

/// Whether an inline marker at document range `[ms, me)` should be revealed
/// (shown raw) — the Obsidian behavior of only showing markup for the element
/// the caret or selection is actually touching. The marker reveals when the
/// caret sits anywhere within its smallest enclosing inline span, or when a
/// selection overlaps that span (starts inside, ends inside, or wraps it).
fn marker_revealed(
    ms: usize,
    me: usize,
    spans: &[Span],
    cursor: usize,
    sel: Option<(usize, usize)>,
) -> bool {
    // Smallest span containing the marker is the element it belongs to.
    let mut best: Option<(usize, usize)> = None;
    for s in spans {
        if s.start <= ms && me <= s.end {
            if best.is_none_or(|(bs, be)| (s.end - s.start) < (be - bs)) {
                best = Some((s.start, s.end));
            }
        }
    }
    let (a, b) = best.unwrap_or((ms, me));
    if a <= cursor && cursor <= b {
        return true;
    }
    matches!(sel, Some((ss, se)) if ss <= b && se >= a)
}

/// Line-local ranges of the syntax markers that are *revealed* on this line
/// (heading `#`s when the caret is on the line, plus inline markers whose span
/// the caret/selection touches), so they can be dimmed like Obsidian's gray
/// markup. Concealed markers aren't included (they're hidden, not dimmed).
fn dim_ranges(
    line: &str,
    line_start: usize,
    markers: &[(usize, usize)],
    spans: &[Span],
    cursor: usize,
    sel: Option<(usize, usize)>,
) -> Vec<(usize, usize)> {
    let (ls, le) = (line_start, line_start + line.len());
    let on_line = ls <= cursor && cursor <= le;
    let mut ranges = Vec::new();
    if on_line {
        if let Some(n) = heading_prefix_len(line) {
            ranges.push((0, n));
        }
    }
    for &(ms, me) in markers {
        if me > ls && ms < le && marker_revealed(ms, me, spans, cursor, sel) {
            let a = ms.max(ls) - ls;
            let b = me.min(le) - ls;
            if a < b {
                ranges.push((a, b));
            }
        }
    }
    ranges
}

/// Detect a bullet/task list line and report the marker + optional checkbox
/// ranges (all line-local byte offsets).
fn parse_list_line(line: &str) -> Option<ListInfo> {
    let indent = line.len() - line.trim_start().len();
    let marker = line[indent..].chars().next()?;
    if !matches!(marker, '-' | '*' | '+') {
        return None;
    }
    let after_marker = indent + marker.len_utf8();
    if !line[after_marker..].starts_with(' ') {
        return None; // a marker must be followed by a space
    }
    let mut i = after_marker;
    while line[i..].starts_with(' ') {
        i += 1;
    }
    // `[ ]` / `[x]` immediately after the marker's whitespace, then a space.
    let checkbox = if line.len() >= i + 4
        && line.is_char_boundary(i + 3)
        && &line[i..i + 1] == "["
        && &line[i + 2..i + 3] == "]"
        && line[i + 3..].starts_with(' ')
    {
        Some(Checkbox {
            bracket: i..i + 3,
            checked: line.as_bytes()[i + 1] != b' ',
        })
    } else {
        None
    };
    Some(ListInfo {
        marker: indent..after_marker,
        checkbox,
    })
}

/// Decide how to render a checkbox line given the caret's line-local column
/// (`None` if the caret is on another line). Returns the `- [ ] ` prefix range,
/// whether the box is checked, and whether the glyph should be shown (vs raw).
/// Like Obsidian, the raw markdown is revealed only when the caret is *next to*
/// the checkbox — not merely somewhere on the line.
fn checkbox_render(line: &str, cursor_col: Option<usize>) -> Option<(Range<usize>, bool, bool)> {
    let info = parse_list_line(line)?;
    let cb = info.checkbox?;
    let prefix_end = cb.bracket.end + 1; // through the space after `]`
    let near = cursor_col.is_some_and(|c| c >= info.marker.start && c < prefix_end);
    Some((info.marker.start..prefix_end, cb.checked, !near))
}

/// The display transforms for a line. Block-level markup (heading `#`, bullet)
/// reveals when the caret is anywhere on the line; a checkbox reveals only when
/// the caret is next to it. Inline markers (`**`/`*`/`` ` ``/link brackets)
/// reveal per-span: each shows raw only when the caret or selection touches its
/// own element (see [`marker_revealed`]) — otherwise it stays concealed even
/// while other parts of the same line are being edited.
fn line_ops(
    line: &str,
    line_start: usize,
    markers: &[(usize, usize)],
    spans: &[Span],
    cursor: usize,
    sel: Option<(usize, usize)>,
) -> Vec<Op> {
    let (ls, le) = (line_start, line_start + line.len());
    let on_line = ls <= cursor && cursor <= le;
    let cursor_col = on_line.then(|| cursor - ls);
    let mut ops: Vec<Op> = Vec::new();

    if let Some(n) = heading_prefix_len(line) {
        if !on_line {
            ops.push(Op { src: 0..n, repl: Repl::Hide });
        }
    } else if let Some((range, checked, shown)) = checkbox_render(line, cursor_col) {
        if shown {
            ops.push(Op { src: range, repl: Repl::Checkbox(checked) });
        }
    } else if !on_line {
        // Plain bullet: swap the marker char for a dot.
        if let Some(info) = parse_list_line(line) {
            ops.push(Op { src: info.marker.clone(), repl: Repl::Bullet });
        }
    }

    for &(ms, me) in markers {
        if me > ls && ms < le && !marker_revealed(ms, me, spans, cursor, sel) {
            let a = ms.max(ls) - ls;
            let b = me.min(le) - ls;
            if a < b && !ops.iter().any(|o| a < o.src.end && o.src.start < b) {
                ops.push(Op { src: a..b, repl: Repl::Hide });
            }
        }
    }

    ops.sort_by_key(|o| o.src.start);
    ops
}

/// An image referenced on its own line: a markdown path or an Obsidian embed.
enum ImageRef {
    Rel(String),   // ![alt](path) — relative to the note folder
    Embed(String), // ![[name]] — resolved vault-wide by filename
}

/// If a line is solely an image — markdown `![alt](path)` or Obsidian
/// `![[name.png]]` (optionally `![[name.png|size]]`) — return its reference.
fn parse_image_line(line: &str) -> Option<ImageRef> {
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

const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Walk the vault once, mapping each image file's lowercased basename to its
/// path (shortest path wins on collision, matching Obsidian's preference).
fn build_vault_index(vault: Option<&Path>) -> HashMap<String, PathBuf> {
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
            let is_image = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false);
            if !is_image {
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
fn load_image(path: &Path) -> Option<Arc<RenderImage>> {
    let mut rgba = image::open(path).ok()?.into_rgba8();
    // GPUI expects BGRA; swap the R and B channels in place.
    for px in rgba.pixels_mut() {
        px.0.swap(0, 2);
    }
    let frame = image::Frame::new(rgba);
    Some(Arc::new(RenderImage::new(vec![frame])))
}

/// Per-line code-fence classification.
#[derive(Clone, Copy)]
struct CodeLine {
    in_block: bool, // inside a fenced block (fence lines included) -> code panel
    is_fence: bool, // the line is a ``` fence
    block: usize,   // 1-based block id (0 when not in a block)
}

/// Whether line `li`'s ``` fence should be concealed: only when it's a fence and
/// the caret is *not* inside that same block (Obsidian reveals fences on edit).
fn fence_hidden(code: &[CodeLine], cursor_line: usize, li: usize) -> bool {
    let cursor_block = if code[cursor_line].in_block {
        code[cursor_line].block
    } else {
        0
    };
    code[li].is_fence && code[li].block != cursor_block
}

/// Classify every line for fenced code blocks. Each block gets a distinct id so
/// callers can tell whether the caret sits in the *same* block as a given line.
/// A table separator row: `| --- | :--: |` — cells of only `-`/`:`/spaces,
/// each containing a `-`.
fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('|') {
        return false;
    }
    let t = t.trim_start_matches('|').trim_end_matches('|');
    !t.is_empty()
        && t.split('|').all(|c| {
            let c = c.trim();
            !c.is_empty() && c.contains('-') && c.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        })
}

/// Inclusive line ranges of GFM tables (a `|` header row immediately followed by
/// a separator row, then contiguous `|` rows).
fn table_blocks(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].contains('|')
            && i + 1 < lines.len()
            && is_table_sep(lines[i + 1])
        {
            let start = i;
            let mut end = i + 1;
            let mut j = i + 2;
            while j < lines.len() && lines[j].contains('|') && !lines[j].trim().is_empty() {
                end = j;
                j += 1;
            }
            blocks.push((start, end));
            i = j;
        } else {
            i += 1;
        }
    }
    blocks
}

/// Split a table row into trimmed cell strings (dropping the outer pipes).
fn split_table_row(line: &str) -> Vec<String> {
    table_cell_spans(line).iter().map(|r| line[r.clone()].to_string()).collect()
}

/// Line-local byte ranges of each cell's *trimmed content* (mirrors
/// [`split_table_row`] but keeps source positions, so a rendered cell can map
/// clicks/caret back to the raw markdown for in-cell editing).
fn table_cell_spans(line: &str) -> Vec<Range<usize>> {
    let mut start = line.len() - line.trim_start().len();
    let mut end = line.trim_end().len();
    if line[start..end].starts_with('|') {
        start += 1;
    }
    if end > start && line[start..end].ends_with('|') {
        end -= 1;
    }
    let mut spans = Vec::new();
    let mut pos = start;
    for part in line[start..end].split('|') {
        let seg_end = pos + part.len();
        let cs = pos + (part.len() - part.trim_start().len());
        let ce = seg_end - (part.len() - part.trim_end().len());
        spans.push(cs..ce.max(cs));
        pos = seg_end + 1; // skip the '|'
    }
    spans
}

/// Per-column alignment from the separator row (`:--` left, `--:` right, `:-:`).
fn table_aligns(sep: &str) -> Vec<TableAlign> {
    split_table_row(sep)
        .iter()
        .map(|c| {
            let c = c.trim();
            match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => TableAlign::Center,
                (false, true) => TableAlign::Right,
                _ => TableAlign::Left,
            }
        })
        .collect()
}

/// A structural edit on a table, from the cell context menu.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableOp {
    RowAbove,
    RowBelow,
    ColLeft,
    ColRight,
    DeleteRow,
    DeleteColumn,
}

fn render_table_row(cells: &[String]) -> String {
    let mut s = String::from("|");
    for c in cells {
        s.push(' ');
        s.push_str(c);
        s.push_str(" |");
    }
    s
}

fn render_table_sep(aligns: &[TableAlign]) -> String {
    let mut s = String::from("|");
    for a in aligns {
        s.push_str(match a {
            TableAlign::Left => " --- ",
            TableAlign::Center => " :-: ",
            TableAlign::Right => " --: ",
        });
        s.push('|');
    }
    s
}

/// Apply a structural edit to the table containing `line_idx`, targeting column
/// `col`. Returns the new document text plus the cell (line, column) to focus
/// afterward, or `None` if the edit isn't valid (e.g. deleting the header row or
/// the last column). The table is re-emitted normalized (uniform spacing).
fn edit_table(text: &str, line_idx: usize, col: usize, op: TableOp) -> Option<(String, usize, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let (start, end) = table_blocks(&lines)
        .into_iter()
        .find(|(s, e)| (*s..=*e).contains(&line_idx))?;
    if line_idx == start + 1 {
        return None; // the separator row isn't a target
    }
    // Grid of cell contents for header + body rows (separator excluded).
    let row_lis: Vec<usize> = (start..=end).filter(|&i| i != start + 1).collect();
    let mut grid: Vec<Vec<String>> = row_lis.iter().map(|&i| split_table_row(lines[i])).collect();
    let mut aligns = table_aligns(lines[start + 1]);
    let ncols = grid
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(1)
        .max(aligns.len())
        .max(1);
    for r in &mut grid {
        r.resize(ncols, String::new());
    }
    aligns.resize(ncols, TableAlign::Left);
    // Grid row index of the targeted line (0 = header).
    let target_row = row_lis.iter().position(|&i| i == line_idx).unwrap_or(0);

    // (focus grid-row, focus column) after the edit.
    let (focus_grow, focus_col) = match op {
        TableOp::RowAbove | TableOp::RowBelow => {
            // New rows always land in the body (never above the header).
            let g = if op == TableOp::RowBelow { target_row + 1 } else { target_row.max(1) };
            grid.insert(g, vec![String::new(); ncols]);
            (g, 0)
        }
        TableOp::DeleteRow => {
            if target_row == 0 || grid.len() <= 1 {
                return None; // keep the header row
            }
            grid.remove(target_row);
            (target_row.min(grid.len() - 1).max(1), 0)
        }
        TableOp::ColLeft | TableOp::ColRight => {
            let at = if op == TableOp::ColRight { col + 1 } else { col }.min(ncols);
            for r in &mut grid {
                r.insert(at, String::new());
            }
            aligns.insert(at, TableAlign::Left);
            (target_row, at)
        }
        TableOp::DeleteColumn => {
            if ncols <= 1 || col >= ncols {
                return None; // keep at least one column
            }
            for r in &mut grid {
                r.remove(col);
            }
            aligns.remove(col);
            (target_row, col.min(ncols - 2))
        }
    };

    // Re-emit: header, separator, then body rows.
    let mut block = Vec::with_capacity(grid.len() + 1);
    block.push(render_table_row(&grid[0]));
    block.push(render_table_sep(&aligns));
    for r in &grid[1..] {
        block.push(render_table_row(r));
    }
    // Map the focus grid-row to an output line index (row 0 = header at `start`,
    // body rows follow the separator).
    let focus_line = if focus_grow == 0 { start } else { start + 1 + focus_grow };

    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    out.splice(start..=end, block);
    Some((out.join("\n"), focus_line, focus_col))
}

/// The line index of the closing `---` of YAML frontmatter (a `---` fence on the
/// very first line), if present. Those lines render as a de-emphasized panel.
fn frontmatter_end(text: &str) -> Option<usize> {
    if text.split('\n').next().map(str::trim) != Some("---") {
        return None;
    }
    text.split('\n')
        .enumerate()
        .skip(1)
        .find(|(_, l)| l.trim() == "---")
        .map(|(i, _)| i)
}

/// The bundled syntax set, loaded once. Uses `two-face`'s extended (bat-curated)
/// set — ~200 languages, including TypeScript/TSX, TOML, Nix, etc. that
/// syntect's own defaults omit. `_newlines` because we feed each line to the
/// highlighter with its trailing `\n`.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// The dark theme used to color code. Only the foreground colors are used; the
/// block's background is our own code panel.
fn code_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let mut ts = ThemeSet::load_defaults();
        ts.themes
            .remove("base16-ocean.dark")
            .or_else(|| ts.themes.values().next().cloned())
            .expect("at least one default theme")
    })
}

fn conv_color(c: syntect::highlighting::Color) -> Hsla {
    Rgba {
        r: c.r as f32 / 255.,
        g: c.g as f32 / 255.,
        b: c.b as f32 / 255.,
        a: c.a as f32 / 255.,
    }
    .into()
}

/// Syntax-highlight every fenced code block, returning per-line color runs
/// (line index -> list of `(byte_len, color)` covering that line's source). The
/// opening fence's info string (e.g. ```` ```go ````) selects the language;
/// unknown or missing languages fall back to plain text. Fence lines are not
/// highlighted (they're concealed or shown as raw markers).
fn highlight_code(text: &str, code: &[CodeLine]) -> HashMap<usize, Vec<(usize, Hsla)>> {
    let ss = syntax_set();
    let theme = code_theme();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut map: HashMap<usize, Vec<(usize, Hsla)>> = HashMap::new();

    let mut i = 0;
    while i < lines.len() {
        // An opening fence: a fence line whose block differs from the line above.
        let opens = code[i].is_fence
            && code[i].in_block
            && (i == 0 || code[i - 1].block != code[i].block);
        if !opens {
            i += 1;
            continue;
        }
        let lang = lines[i]
            .trim_start()
            .trim_start_matches('`')
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .trim();
        let syntax = (!lang.is_empty())
            .then(|| ss.find_syntax_by_token(lang))
            .flatten()
            .unwrap_or_else(|| ss.find_syntax_plain_text());
        let mut h = HighlightLines::new(syntax, theme);
        let mut j = i + 1;
        while j < lines.len() && !code[j].is_fence {
            let with_nl = format!("{}\n", lines[j]);
            if let Ok(ranges) = h.highlight_line(&with_nl, ss) {
                let mut runs: Vec<(usize, Hsla)> = Vec::new();
                let mut consumed = 0usize;
                for (style, piece) in ranges {
                    // Clamp away the synthetic trailing newline.
                    let len = piece.len().min(lines[j].len().saturating_sub(consumed));
                    if len == 0 {
                        continue;
                    }
                    runs.push((len, conv_color(style.foreground)));
                    consumed += len;
                }
                map.insert(j, runs);
            }
            j += 1;
        }
        i = j; // resume at the closing fence (or end of doc)
    }
    map
}

fn classify_code(text: &str) -> Vec<CodeLine> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut block = 0usize;
    for line in text.split('\n') {
        let is_fence = line.trim_start().starts_with("```");
        if is_fence {
            if !inside {
                inside = true;
                block += 1;
                out.push(CodeLine { in_block: true, is_fence: true, block });
            } else {
                out.push(CodeLine { in_block: true, is_fence: true, block });
                inside = false;
            }
        } else {
            out.push(CodeLine {
                in_block: inside,
                is_fence: false,
                block: if inside { block } else { 0 },
            });
        }
    }
    out
}

/// Build the display string, its text runs, the source↔display segment map, and
/// any clickable checkbox glyphs for one line, applying the given `ops`.
fn build_line(
    line: &str,
    line_start: usize,
    ops: &[Op],
    spans: &[Span],
    base_font: &Font,
    dim: &[(usize, usize)],
    base_weight: FontWeight,
    checked: bool,
) -> (String, Vec<TextRun>, Vec<Seg>, Vec<CheckHit>) {
    let mut display = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    let mut checks: Vec<CheckHit> = Vec::new();
    let mut pos = 0usize; // line-local source
    let mut dpos = 0usize; // display
    let mut oi = 0usize;

    while pos < line.len() {
        if oi < ops.len() && ops[oi].src.start == pos {
            let op = &ops[oi];
            let dstart = dpos;
            match &op.repl {
                Repl::Hide => {}
                Repl::Bullet => {
                    display.push_str(BULLET);
                    runs.push(bullet_run(BULLET.len(), base_font));
                    dpos += BULLET.len();
                }
                Repl::Checkbox(checked) => {
                    // Reserve a blank slot; the box itself is painted over it.
                    display.push_str(CHECKBOX_SLOT);
                    runs.push(copy_run(
                        StyleKey::default(),
                        CHECKBOX_SLOT.len(),
                        base_font,
                        base_weight,
                        false,
                        false,
                    ));
                    let box_end = dstart + "\u{2003}".len(); // just the em-space
                    dpos += CHECKBOX_SLOT.len();
                    if let Some(k) = line[op.src.clone()].find('[') {
                        checks.push(CheckHit {
                            inner_abs: line_start + op.src.start + k + 1,
                            disp: dstart..box_end,
                            checked: *checked,
                        });
                    }
                }
            }
            segs.push(Seg { src: pos..op.src.end, disp: dstart..dpos, sub: true });
            pos = op.src.end;
            oi += 1;
            continue;
        }
        let next = if oi < ops.len() {
            ops[oi].src.start
        } else {
            line.len()
        };
        let dstart = dpos;
        let mut p = pos;
        let mut cur: Option<(StyleKey, bool, usize)> = None; // (key, dimmed, byte len)
        while p < next {
            let chlen = line[p..].chars().next().unwrap().len_utf8();
            let key = style_at(line_start + p, spans);
            let dimmed = dim.iter().any(|&(a, b)| a <= p && p < b);
            match &mut cur {
                Some((k, d, l)) if *k == key && *d == dimmed => *l += chlen,
                _ => {
                    if let Some((k, d, l)) = cur.take() {
                        runs.push(copy_run(k, l, base_font, base_weight, checked, d));
                    }
                    cur = Some((key, dimmed, chlen));
                }
            }
            display.push_str(&line[p..p + chlen]);
            p += chlen;
        }
        if let Some((k, d, l)) = cur.take() {
            runs.push(copy_run(k, l, base_font, base_weight, checked, d));
        }
        dpos += next - pos;
        segs.push(Seg { src: pos..next, disp: dstart..dpos, sub: false });
        pos = next;
    }

    (display, runs, segs, checks)
}

/// Build one table cell for rendering: its display string, styled runs, and the
/// source↔display segment map (for caret/click mapping). Inline markdown is
/// styled like body text, and — since the caret can sit inside a cell — markers
/// reveal per-span exactly as on a normal line. `cell_doc_start` is the document
/// offset of the cell's content, so `cursor`/`selection` (document offsets) line
/// up with the parsed spans.
fn build_cell(
    cell: &str,
    cell_doc_start: usize,
    base_font: &Font,
    weight: FontWeight,
    cursor: usize,
    sel: Option<(usize, usize)>,
) -> (String, Vec<TextRun>, Vec<Seg>) {
    let cds = cell_doc_start;
    let (raw_spans, raw_markers, _links) = parse(cell);
    // Shift the cell-local parse into document coordinates.
    let spans: Vec<Span> = raw_spans
        .iter()
        .map(|s| Span { start: s.start + cds, end: s.end + cds, attr: s.attr })
        .collect();
    let markers: Vec<(usize, usize)> =
        raw_markers.iter().map(|&(a, b)| (a + cds, b + cds)).collect();
    let ops = line_ops(cell, cds, &markers, &spans, cursor, sel);
    let dim = dim_ranges(cell, cds, &markers, &spans, cursor, sel);
    let (display, runs, segs, _checks) =
        build_line(cell, cds, &ops, &spans, base_font, &dim, weight, false);
    (display, runs, segs)
}

/// Build a fenced-code-block content line: the source rendered verbatim (no
/// concealment, identity source↔display map) in the mono font, colored by the
/// pre-computed syntax-highlight runs. Any tail not covered by `hl` falls back
/// to the default text color.
fn build_code_line(
    line: &str,
    font: &Font,
    hl: &[(usize, Hsla)],
) -> (String, Vec<TextRun>, Vec<Seg>, Vec<CheckHit>) {
    let mono_run = |len: usize, color: Hsla| TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs: Vec<TextRun> = Vec::new();
    let mut consumed = 0usize;
    for &(len, color) in hl {
        let len = len.min(line.len().saturating_sub(consumed));
        if len == 0 {
            continue;
        }
        runs.push(mono_run(len, color));
        consumed += len;
    }
    if consumed < line.len() {
        runs.push(mono_run(line.len() - consumed, rgb(TEXT).into()));
    }
    let segs = vec![Seg { src: 0..line.len(), disp: 0..line.len(), sub: false }];
    (line.to_string(), runs, segs, Vec::new())
}

fn copy_run(
    key: StyleKey,
    len: usize,
    base_font: &Font,
    base_weight: FontWeight,
    checked: bool,
    dim: bool,
) -> TextRun {
    let mut run = text_run(key, len, base_font, base_weight);
    // A dimmed run is a revealed syntax marker (gray, no link underline).
    if dim {
        run.color = rgb(MUTED).into();
        run.underline = None;
    }
    if checked {
        run.color = rgb(MUTED).into();
        run.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(rgb(MUTED).into()),
        });
    }
    run
}

fn bullet_run(len: usize, base_font: &Font) -> TextRun {
    TextRun {
        len,
        font: base_font.clone(),
        color: rgb(MUTED).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

fn line_height(size: Pixels) -> Pixels {
    size * 1.5_f32
}

/// Map a source (line-local) byte offset to its display byte offset via segs.
fn src_to_disp(segs: &[Seg], src: usize) -> usize {
    for s in segs {
        if src < s.src.end {
            return if s.sub {
                s.disp.start
            } else {
                s.disp.start + (src - s.src.start)
            };
        }
    }
    segs.last().map(|s| s.disp.end).unwrap_or(0)
}

/// Map a display byte offset back to a source (line-local) byte offset via segs.
fn disp_to_src(segs: &[Seg], disp: usize) -> usize {
    for s in segs {
        if disp < s.disp.end {
            return if s.sub {
                s.src.start
            } else {
                s.src.start + (disp - s.disp.start)
            };
        }
    }
    segs.last().map(|s| s.src.end).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Text/offset helpers (byte offsets, char-boundary aware).
// ---------------------------------------------------------------------------

fn prev_boundary(text: &str, offset: usize) -> usize {
    text[..offset]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// The byte range of the "word" at `offset`, for double-click selection: the
/// run of alphanumeric/`_` characters the click touches (on either side), or —
/// if it touches none — the single character under the cursor.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
    let offset = offset.min(text.len());
    let right = text[offset..].chars().next();
    let left = text[..offset].chars().next_back();
    if !(right.is_some_and(is_word) || left.is_some_and(is_word)) {
        return match (right, left) {
            (Some(c), _) => offset..offset + c.len_utf8(),
            (None, Some(c)) => offset - c.len_utf8()..offset,
            (None, None) => offset..offset,
        };
    }
    let mut start = offset;
    while let Some(c) = text[..start].chars().next_back() {
        if !is_word(c) {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = offset;
    while let Some(c) = text[end..].chars().next() {
        if !is_word(c) {
            break;
        }
        end += c.len_utf8();
    }
    start..end
}

fn offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let mut line = 0;
    let mut line_start = 0;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + ch.len_utf8();
        }
    }
    (line, offset - line_start)
}

fn line_start(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut n = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            n += 1;
            if n == line {
                return i + 1;
            }
        }
    }
    text.len()
}

fn line_str(text: &str, line: usize) -> &str {
    text.split('\n').nth(line).unwrap_or("")
}

fn line_col_to_offset(text: &str, line: usize, col_byte: usize) -> usize {
    let start = line_start(text, line);
    let l = line_str(text, line);
    let mut col = col_byte.min(l.len());
    while col > 0 && !l.is_char_boundary(col) {
        col -= 1;
    }
    start + col
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_font() -> Font {
        Font {
            family: "Arial".into(),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        }
    }

    /// Return, for each run, the substring it covers plus its resolved styling.
    fn runs_of(line: &str) -> Vec<(String, FontWeight, FontStyle, bool)> {
        let (spans, markers, _links) = parse(line);
        // Caret off the line (beyond its end) so all markers stay concealed.
        let ops = line_ops(line, 0, &markers, &spans, line.len() + 1, None);
        let base = test_font();
        let (display, runs, _segs, _checks) =
            build_line(line, 0, &ops, &spans, &base, &[], FontWeight::NORMAL, false);
        let mut out = Vec::new();
        let mut pos = 0;
        for r in &runs {
            out.push((
                display[pos..pos + r.len].to_string(),
                r.font.weight,
                r.font.style,
                r.background_color.is_some(),
            ));
            pos += r.len;
        }
        out
    }

    #[test]
    fn bold_italic_code_spans_apply() {
        let runs = runs_of("A **live** and *inline* and `code` end");
        let bold = runs.iter().find(|r| r.0 == "live").unwrap();
        assert_eq!(bold.1, FontWeight::BOLD);
        let italic = runs.iter().find(|r| r.0 == "inline").unwrap();
        assert_eq!(italic.2, FontStyle::Italic);
        let code = runs.iter().find(|r| r.0 == "code").unwrap();
        assert!(code.3, "inline code should have a background");
        // Markers are concealed: the display never contains ** * or `.
        let display: String = runs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(display, "A live and inline and code end");
    }

    #[test]
    fn table_cell_renders_inline_markdown() {
        // Caret off the cell (large offset): markers concealed like body text.
        let far = 10_000;
        // A cell with an inline code span: backticks concealed, code background.
        let (display, runs, _s) =
            build_cell("`postgres`", 0, &test_font(), FontWeight::NORMAL, far, None);
        assert_eq!(display, "postgres");
        assert!(
            runs.iter().any(|r| r.background_color.is_some()),
            "code cell should have a background run"
        );
        // A header cell keeps its base weight bold while styling its content.
        let (disp, runs, _s) = build_cell("**Role**", 0, &test_font(), FontWeight::BOLD, far, None);
        assert_eq!(disp, "Role");
        assert!(runs.iter().all(|r| r.font.weight == FontWeight::BOLD));
    }

    #[test]
    fn table_structural_edits() {
        let t = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        // Insert a row below the body row (line 2).
        let (out, fl, fc) = edit_table(t, 2, 0, TableOp::RowBelow).unwrap();
        assert_eq!(out, "| a | b |\n| --- | --- |\n| 1 | 2 |\n|  |  |");
        assert_eq!((fl, fc), (3, 0));
        // Insert a column to the right of column 0.
        let (out, _, fc) = edit_table(t, 0, 0, TableOp::ColRight).unwrap();
        assert_eq!(out, "| a |  | b |\n| --- | --- | --- |\n| 1 |  | 2 |");
        assert_eq!(fc, 1);
        // Insert a column to the left of column 1.
        let (out, _, _) = edit_table(t, 2, 1, TableOp::ColLeft).unwrap();
        assert_eq!(out, "| a |  | b |\n| --- | --- | --- |\n| 1 |  | 2 |");
        // Delete the body row.
        let (out, _, _) = edit_table(t, 2, 0, TableOp::DeleteRow).unwrap();
        assert_eq!(out, "| a | b |\n| --- | --- |");
        // Delete column 1.
        let (out, _, _) = edit_table(t, 0, 1, TableOp::DeleteColumn).unwrap();
        assert_eq!(out, "| a |\n| --- |\n| 1 |");
        // Guards: can't delete the header row or the only column.
        assert!(edit_table(t, 0, 0, TableOp::DeleteRow).is_none());
        let one = "| a |\n| --- |\n| 1 |";
        assert!(edit_table(one, 2, 0, TableOp::DeleteColumn).is_none());
    }

    #[test]
    fn table_cell_spans_map_source() {
        let line = "| `a` | bb |";
        let spans = table_cell_spans(line);
        assert_eq!(spans.len(), 2);
        assert_eq!(&line[spans[0].clone()], "`a`");
        assert_eq!(&line[spans[1].clone()], "bb");
    }

    #[test]
    fn code_line_is_syntax_colored() {
        let text = "```rust\nfn main() {}\n```";
        let code = classify_code(text);
        let hl = highlight_code(text, &code);
        // Line 1 is the code content; it should have colored runs.
        let runs = hl.get(&1).expect("code line highlighted");
        assert!(!runs.is_empty());
        // Runs cover exactly the source line's bytes (no newline leakage).
        let total: usize = runs.iter().map(|(len, _)| len).sum();
        assert_eq!(total, "fn main() {}".len());
        // Fence lines are not highlighted.
        assert!(hl.get(&0).is_none() && hl.get(&2).is_none());
    }

    #[test]
    fn build_code_line_covers_whole_line() {
        let line = "let x = 1;";
        let (display, runs, segs, _checks) =
            build_code_line(line, &test_font(), &[(3, gpui::black()), (2, gpui::white())]);
        assert_eq!(display, line);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, line.len(), "runs must span the full line");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn double_click_word_selection() {
        let text = "hello world_2 , foo";
        // Click inside "hello".
        assert_eq!(word_at(text, 2), 0..5);
        // Click at the trailing boundary of a word still selects that word.
        assert_eq!(word_at(text, 5), 0..5);
        // Underscores and digits are part of the word.
        assert_eq!(word_at(text, 8), 6..13);
        // Isolated punctuation (non-word on both sides) selects just that char.
        assert_eq!(word_at(text, 14), 14..15); // the comma
        // End of buffer falls back to the char on the left ("foo").
        assert_eq!(word_at(text, text.len()), 16..19);
    }

    #[test]
    fn fences_reveal_only_with_caret_inside() {
        // lines: 0 para, 1 ```rust, 2 code, 3 ```, 4 para
        let text = "hi\n```rust\nfn x() {}\n```\nbye";
        let code = classify_code(text);
        assert!(code[1].is_fence && code[3].is_fence);
        assert!(code[2].in_block && !code[2].is_fence);
        // Caret on line 0 (outside): both fences hidden.
        assert!(fence_hidden(&code, 0, 1));
        assert!(fence_hidden(&code, 0, 3));
        // Caret on line 2 (inside the block): fences shown.
        assert!(!fence_hidden(&code, 2, 1));
        assert!(!fence_hidden(&code, 2, 3));
        // Caret on a fence line itself counts as inside.
        assert!(!fence_hidden(&code, 1, 3));
    }

    #[test]
    fn checkbox_reveals_only_next_to_box() {
        let line = "- [x] do the thing";
        // marker `-`=0, `[x]`=2..5, prefix (through the space) ends at col 6.
        // Caret elsewhere on the line (in the text) -> glyph shown.
        let (_r, checked, shown) = checkbox_render(line, Some(10)).unwrap();
        assert!(checked && shown);
        // Caret on another line -> glyph shown.
        assert!(checkbox_render(line, None).unwrap().2);
        // Caret next to the box (within `- [x] `) -> raw shown.
        assert!(!checkbox_render(line, Some(0)).unwrap().2);
        assert!(!checkbox_render(line, Some(3)).unwrap().2);
        assert!(!checkbox_render(line, Some(5)).unwrap().2);
        // First text column (6) is past the prefix -> glyph stays.
        assert!(checkbox_render(line, Some(6)).unwrap().2);
    }

    #[test]
    fn image_line_parsing() {
        assert!(matches!(parse_image_line("![alt](pic.png)"), Some(ImageRef::Rel(p)) if p == "pic.png"));
        assert!(matches!(parse_image_line("![[att.png]]"), Some(ImageRef::Embed(n)) if n == "att.png"));
        // Obsidian size/alt suffix is stripped for resolution.
        assert!(matches!(parse_image_line("![[att.png|300]]"), Some(ImageRef::Embed(n)) if n == "att.png"));
        // Remote images and plain text are not treated as image lines.
        assert!(parse_image_line("![x](https://e.com/a.png)").is_none());
        assert!(parse_image_line("just text").is_none());
    }

    #[test]
    fn link_urls_are_captured_and_hit_tested() {
        let line = "see [the docs](https://obsidian.md) now";
        let (_spans, _markers, links) = parse(line);
        assert_eq!(links.len(), 1);
        let (range, target) = &links[0];
        assert!(matches!(target, LinkTarget::Url(u) if u.as_ref() == "https://obsidian.md"));
        // The link's source range covers the whole `[text](url)`.
        assert!(line[range.clone()].starts_with("[the docs]"));
    }

    #[test]
    fn highlight_and_tag_style() {
        // ==highlight== conceals the == and shows the text with a background.
        let runs = runs_of("say ==wow== and #tag done");
        let display: String = runs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(display, "say wow and #tag done");
        assert!(runs.iter().find(|r| r.0 == "wow").unwrap().3, "highlight bg");
        // The #tag keeps its `#` and gets a background too.
        assert!(runs.iter().find(|r| r.0 == "#tag").unwrap().3, "tag bg");
    }

    #[test]
    fn inline_reveal_is_per_span() {
        let line = "a `code` and **bold** end";
        let (spans, markers, _l) = parse(line);
        let base = test_font();
        let build = |cursor: usize, sel: Option<(usize, usize)>| {
            let ops = line_ops(line, 0, &markers, &spans, cursor, sel);
            build_line(line, 0, &ops, &spans, &base, &[], FontWeight::NORMAL, false).0
        };
        let code_pos = line.find("code").unwrap();
        let bold_pos = line.find("bold").unwrap();
        // Caret in the code span: only its backticks reveal.
        assert!(build(code_pos, None).contains('`'));
        assert!(!build(code_pos, None).contains("**"));
        // Caret in the bold span: only its `**` reveal.
        assert!(build(bold_pos, None).contains("**"));
        assert!(!build(bold_pos, None).contains('`'));
        // Caret on the line but outside every span: all concealed.
        assert_eq!(build(line.len(), None), "a code and bold end");
        // A selection overlapping the code span reveals it even if the caret is
        // elsewhere.
        assert!(build(line.len(), Some((code_pos, code_pos + 1))).contains('`'));
    }

    #[test]
    fn per_line_span_bucketing_matches_global() {
        // The perf refactor buckets spans/markers per source line before building.
        // That must be indistinguishable from scanning the global sets, for every
        // line — otherwise concealment/styling would silently differ.
        let text =
            "**bold** and *it*\nplain `code` here\n# Heading\n- [x] a ==hi== #tag\n> quote **b**";
        let (spans, markers, _l) = parse(text);
        let base = test_font();
        let mut off = 0usize;
        for line in text.split('\n') {
            let (ls, le) = (off, off + line.len());
            let fspans: Vec<Span> =
                spans.iter().filter(|s| s.start < le && s.end > ls).cloned().collect();
            let fmarks: Vec<(usize, usize)> =
                markers.iter().copied().filter(|m| m.1 > ls && m.0 < le).collect();

            // Exercise both concealed (caret off-line) and revealed (caret at
            // line start) paths; filtered per-line sets must match the global.
            let far = off + line.len() + 1;
            let ops_g = line_ops(line, off, &markers, &spans, far, None);
            let ops_f = line_ops(line, off, &fmarks, &fspans, far, None);
            let (dg, rg, _, _) =
                build_line(line, off, &ops_g, &spans, &base, &[], FontWeight::NORMAL, false);
            let (df, rf, _, _) =
                build_line(line, off, &ops_f, &fspans, &base, &[], FontWeight::NORMAL, false);

            assert_eq!(dg, df, "display differs for {line:?}");
            assert_eq!(rg.len(), rf.len(), "run count differs for {line:?}");
            for (a, b) in rg.iter().zip(rf.iter()) {
                assert_eq!(a.len, b.len);
                assert_eq!(a.font.weight, b.font.weight);
                assert_eq!(a.font.style, b.font.style);
                assert_eq!(a.background_color.is_some(), b.background_color.is_some());
            }
            off = le + 1;
        }
    }

    #[test]
    fn wikilinks_conceal_and_target() {
        // Plain: shows the target, conceals the brackets.
        let runs = runs_of("go to [[Some Note]] now");
        let display: String = runs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(display, "go to Some Note now");
        // Alias + path + heading: shows alias, target resolves to the note name.
        let (_s, _m, links) = parse("see [[folder/Note#Sec|the alias]] end");
        assert_eq!(links.len(), 1);
        assert!(matches!(&links[0].1, LinkTarget::Note(n) if n.as_ref() == "folder/Note"));
        let runs = runs_of("see [[folder/Note#Sec|the alias]] end");
        let display: String = runs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(display, "see the alias end");
        // An ![[embed]] is not treated as an inline wikilink.
        let (_s, _m, links) = parse("![[image.png]]");
        assert!(links.is_empty());
    }
}
