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
use std::sync::Arc;

use gpui::{
    div, fill, point, px, quad, relative, rgb, rgba, size, transparent_black, App, BorderStyle, Bounds,
    ClipboardItem, ContentMask, Context, Corners, DispatchPhase, Element, ElementId, Entity,
    EventEmitter, FocusHandle, Font, FontStyle, FontWeight, GlobalElementId, InspectorElementId,
    InteractiveElement, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Size, StrikethroughStyle, Style, Styled, TextAlign, TextRun,
    UnderlineStyle, WrappedLine, Window,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

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
}

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
        }
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
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
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
                    self.set_cursor(prev_boundary(&self.text, self.cursor), shift);
                }
                false
            }
            "right" => {
                if let Some(ch) = self.text[self.cursor..].chars().next() {
                    self.set_cursor(self.cursor + ch.len_utf8(), shift);
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
                self.set_cursor(line_start(&self.text, line), shift);
                false
            }
            "end" => {
                let (line, _) = offset_to_line_col(&self.text, self.cursor);
                let p = line_start(&self.text, line) + line_str(&self.text, line).len();
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
        let l = self
            .lines
            .iter()
            .find(|l| cursor >= l.start && cursor <= l.start + l.source.len())?;
        let disp = src_to_disp(&l.segs, cursor - l.start);
        let p = l
            .wrapped
            .position_for_index(disp, l.line_height)
            .unwrap_or_default();
        Some((p.x + l.x_offset(p.y), l.top + p.y, l.line_height))
    }
}

// ---------------------------------------------------------------------------
// The custom element: shapes + wraps lines, paints text/selection/caret with
// scrolling, and routes mouse + wheel events back to the editor entity.
// ---------------------------------------------------------------------------

struct EditorElement {
    editor: Entity<MarkdownEditor>,
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
        let (text, cursor, selection, mut scroll_top, scroll_to_caret) = {
            let e = self.editor.read(cx);
            (
                e.text.clone(),
                e.cursor,
                e.selection().map(|r| (r.start, r.end)),
                e.scroll_top,
                e.scroll_to_caret,
            )
        };

        let mut base_font = window.text_style().font();
        base_font.family = UI_FONT.into();
        let mut mono_font = base_font.clone();
        mono_font.family = MONO_FONT.into();
        let (spans, markers, links) = parse(&text);
        let (cursor_line, _) = offset_to_line_col(&text, cursor);
        let wrap_width = bounds.size.width.max(px(80.));
        let code = classify_code(&text);
        // YAML frontmatter: a `---` fence at the very top. Shown de-emphasized as
        // a metadata panel (reusing the code-block presentation).
        let fm_end: Option<usize> = if text.split('\n').next().map(str::trim) == Some("---") {
            text.split('\n')
                .enumerate()
                .skip(1)
                .find(|(_, l)| l.trim() == "---")
                .map(|(i, _)| i)
        } else {
            None
        };

        // Pre-shape rendered tables (blocks not containing the caret): measure
        // columns, then lay out each row's cells.
        let lines_vec: Vec<&str> = text.split('\n').collect();
        let mut table_map: HashMap<usize, TableRender> = HashMap::new();
        for (start, end) in table_blocks(&lines_vec) {
            if (start..=end).contains(&cursor_line) {
                continue; // caret is inside — show the raw source so it's editable
            }
            let aligns = table_aligns(lines_vec[start + 1]);
            let row_lis: Vec<usize> = (start..=end).filter(|&li| li != start + 1).collect();
            let mut shaped: Vec<Vec<gpui::ShapedLine>> = Vec::new();
            let mut colw: Vec<Pixels> = Vec::new();
            for (ri, &li) in row_lis.iter().enumerate() {
                let mut sr = Vec::new();
                for (c, cell) in split_table_row(lines_vec[li]).into_iter().enumerate() {
                    let mut f = base_font.clone();
                    if ri == 0 {
                        f.weight = FontWeight::BOLD; // header row
                    }
                    let run = TextRun {
                        len: cell.len(),
                        font: f,
                        color: rgb(TEXT).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let sl =
                        window.text_system().shape_line(cell.into(), px(TABLE_FONT), &[run], None);
                    if c >= colw.len() {
                        colw.resize(c + 1, px(0.));
                    }
                    colw[c] = colw[c].max(sl.width);
                    sr.push(sl);
                }
                shaped.push(sr);
            }
            for w in &mut colw {
                *w += px(TABLE_PAD_X * 2.);
            }
            let mut colx: Vec<Pixels> = vec![px(0.)];
            for w in &colw {
                colx.push(*colx.last().unwrap() + *w);
            }
            let mut ri = 0;
            for li in start..=end {
                if li == start + 1 {
                    table_map.insert(li, TableRender::Sep);
                    continue;
                }
                let cells = std::mem::take(&mut shaped[ri])
                    .into_iter()
                    .enumerate()
                    .map(|(c, shaped)| TableCell {
                        shaped,
                        x: colx[c],
                        width: colw[c],
                        align: aligns.get(c).copied().unwrap_or(TableAlign::Left),
                    })
                    .collect();
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
                (px(15.), FontWeight::NORMAL)
            } else {
                heading_style(line)
            };
            let lh = line_height(font_size);
            let font = if in_block { &mono_font } else { &base_font };
            // Hide a ``` fence line unless the caret is inside that same block.
            let hide_fence = fence_hidden(&code, cursor_line, li);
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
                let mut v = line_ops(line, offset, &markers, cursor_col);
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
            // On the caret's line the markers are shown raw; dim them (like the
            // gray `#` of a heading in Obsidian).
            let dim = if is_cursor_line && !in_block {
                let mut d = dim_ranges(line, offset, &markers);
                if let Some((_, plen)) = quote {
                    d.push((0, plen)); // dim the `>` on the line being edited
                }
                d
            } else {
                Vec::new()
            };
            let (display, runs, segs, checks) =
                build_line(line, offset, &ops, &spans, font, &dim, base_weight, checked);
            let display = SharedString::from(display);
            let shape = |width: Pixels, window: &mut Window| {
                window
                    .text_system()
                    .shape_text(display.clone(), font_size, &runs, Some(width.max(px(40.))), None)
                    .expect("shape_text")
                    .into_iter()
                    .next()
                    .expect("one wrapped line")
            };
            let mut wrapped = shape(wrap_width, window);

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
                    wrapped = shape(wrap_width - hang, window);
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
        let viewport_height = bounds.size.height;
        let layout = Rc::new(Layout {
            lines,
            total_height,
            viewport_height,
            links,
        });

        // Keep the caret in view.
        if scroll_to_caret {
            if let Some((_, cy, lh)) = layout.caret(cursor) {
                if cy < scroll_top {
                    scroll_top = cy;
                } else if cy + lh > scroll_top + viewport_height {
                    scroll_top = cy + lh - viewport_height;
                }
            }
        }
        let max_scroll = (total_height - viewport_height).max(px(0.));
        scroll_top = scroll_top.clamp(px(0.), max_scroll);

        self.editor.update(cx, |e, _| {
            e.layout = Some(layout.clone());
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

            // Selection background (per wrapped row).
            if let Some((ss, se)) = prepaint.selection {
                let (ls, le) = (line.start, line.start + line.source.len());
                if se > ls && ss < le {
                    let a = src_to_disp(&line.segs, ss.max(ls) - ls);
                    let b = src_to_disp(&line.segs, se.min(le) - ls);
                    let lh = line.line_height;
                    let pa = line.wrapped.position_for_index(a, lh).unwrap_or_default();
                    let pb = line.wrapped.position_for_index(b, lh).unwrap_or_default();
                    let full_w = line.wrapped.size(lh).width;
                    let paint_row = |window: &mut Window, x0: Pixels, x1: Pixels, row_y: Pixels| {
                        let shift = line.x_offset(row_y);
                        let rect = Bounds {
                            origin: point(bounds.origin.x + x0 + shift, screen_top + row_y),
                            size: size((x1 - x0).max(px(3.)), lh),
                        };
                        window.paint_quad(fill(rect, rgb(SELECTION)));
                    };
                    if pa.y == pb.y {
                        paint_row(window, pa.x, pb.x, pa.y);
                    } else {
                        paint_row(window, pa.x, full_w, pa.y);
                        let mut ry = pa.y + lh;
                        while ry + px(0.5) < pb.y {
                            paint_row(window, px(0.), full_w, ry);
                            ry += lh;
                        }
                        paint_row(window, px(0.), pb.x, pb.y);
                    }
                }
            }

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
                    let tw = cell.shaped.width;
                    let tx = match cell.align {
                        TableAlign::Left => px(TABLE_PAD_X),
                        TableAlign::Right => (cell.width - px(TABLE_PAD_X) - tw).max(px(TABLE_PAD_X)),
                        TableAlign::Center => ((cell.width - tw) / 2.).max(px(TABLE_PAD_X)),
                    };
                    let _ = cell.shaped.paint(
                        point(cx0 + tx, screen_top + px(TABLE_PAD_Y)),
                        line.line_height,
                        window,
                        cx,
                    );
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
                    let _ = line.wrapped.paint(o, lh, TextAlign::Left, None, w, cx);
                });
            } else {
                let o = point(base_x, screen_top);
                let _ = line.wrapped.paint_background(o, lh, TextAlign::Left, None, window, cx);
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
                    let Some(layout) = e.layout.clone() else { return };
                    let x = ev.position.x - bounds.origin.x;
                    let content_y = ev.position.y - bounds.origin.y + e.scroll_top;
                    e.focus_handle.focus(window);
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
        // Scroll wheel.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &ScrollWheelEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || !bounds.contains(&ev.position) {
                    return;
                }
                editor.update(cx, |e, cx| {
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
const TABLE_FONT: f32 = 15.0; // table cell font size (px)
const TABLE_BORDER: u32 = 0x3a3a3a; // cell border color

#[derive(Clone, Copy, PartialEq)]
enum TableAlign {
    Left,
    Center,
    Right,
}

/// A rendered table cell: its shaped text plus column geometry (x/width are
/// relative to the table's left edge).
struct TableCell {
    shaped: gpui::ShapedLine,
    x: Pixels,
    width: Pixels,
    align: TableAlign,
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
        let (size, weight) = match hashes {
            1 => (27., FontWeight::BOLD),
            2 => (21., FontWeight::BOLD),
            3 => (18., FontWeight::SEMIBOLD),
            4 => (16.5, FontWeight::SEMIBOLD),
            5 => (15., FontWeight::SEMIBOLD),
            _ => (14., FontWeight::SEMIBOLD),
        };
        (px(size), weight)
    } else {
        (px(16.), FontWeight::NORMAL)
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

/// Line-local ranges of the syntax markers shown on the caret's line (heading
/// `#`s and inline `**`/`*`/`` ` ``/link brackets), so they can be dimmed. These
/// are the same ranges that get concealed on non-caret lines.
fn dim_ranges(line: &str, line_start: usize, markers: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let (ls, le) = (line_start, line_start + line.len());
    let mut ranges = Vec::new();
    if let Some(n) = heading_prefix_len(line) {
        ranges.push((0, n));
    }
    for &(ms, me) in markers {
        if me > ls && ms < le {
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

/// The display transforms for a line. When the caret is on the line
/// (`cursor_col` is `Some`), headings, inline markers and bullets reveal their
/// raw source, but a checkbox keeps its glyph unless the caret is next to it.
fn line_ops(
    line: &str,
    line_start: usize,
    markers: &[(usize, usize)],
    cursor_col: Option<usize>,
) -> Vec<Op> {
    let on_line = cursor_col.is_some();
    let (ls, le) = (line_start, line_start + line.len());
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

    if !on_line {
        for &(ms, me) in markers {
            if me > ls && ms < le {
                let a = ms.max(ls) - ls;
                let b = me.min(le) - ls;
                if a < b && !ops.iter().any(|o| a < o.src.end && o.src.start < b) {
                    ops.push(Op { src: a..b, repl: Repl::Hide });
                }
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
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
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
        let ops = line_ops(line, 0, &markers, None);
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
