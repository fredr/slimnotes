//! A from-scratch live-preview markdown editor built directly on GPUI.
//!
//! One editor over the whole note. Each line is shaped with its own font size
//! (real heading sizes) and wrap width, inline markdown is styled per span, and
//! the syntax markers are concealed on every line except the one the cursor is
//! on — the Obsidian "Live Preview" behavior.
//!
//! Supports typing, movement, selection, mouse, clipboard, undo/redo, soft-wrap
//! and scrolling.

// Nested `if let ... { if ... }` reads more clearly than a let-chain in several
// of the concealment paths, and the line-building helpers take many small
// positional args by design; both are stylistic lints, silenced crate-wide.
#![allow(clippy::collapsible_if, clippy::too_many_arguments)]

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    anchored, deferred, div, point, px, rgb, size, App, Bounds, ClipboardItem, Context,
    EntityInputHandler, EventEmitter, FocusHandle, InteractiveElement, IntoElement, ParentElement,
    Pixels, Point, Render, RenderImage, SharedString, Styled, UTF16Selection, Window,
};

mod buffer;
mod element;
mod images;
mod input;
mod layout;
mod parse;
mod scrollbar;
mod segments;
mod tables;

use buffer::Buffer;

pub(crate) use element::*;
pub(crate) use images::*;
pub(crate) use input::*;
pub(crate) use layout::*;
pub(crate) use parse::*;
pub(crate) use scrollbar::*;
pub(crate) use segments::*;
pub(crate) use tables::*;

/// Emitted when the buffer content changes (so a host can persist it).
pub enum EditorEvent {
    Changed,
    /// A `[[wikilink]]` was clicked; the host should open the named note.
    OpenNote(String),
}

pub(crate) const TEXT: u32 = 0xdcddde;
pub(crate) const BG: u32 = 0x1e1e1e;
pub(crate) const CARET: u32 = 0x9d86e9;
pub(crate) const LINK: u32 = 0x9d86e9;
pub(crate) const CODE_BG: u32 = 0x323232;
pub(crate) const CODE_PANEL: u32 = 0x282828;
pub(crate) const SELECTION: u32 = 0x4b4488;
pub(crate) const MUTED: u32 = 0x8a8a8a;
pub(crate) const ACCENT: u32 = 0x9d86e9;
pub(crate) const HIGHLIGHT_BG: u32 = 0xd6c04a; // ==highlight== background (highlighter yellow)
pub(crate) const HIGHLIGHT_FG: u32 = 0x1e1e1e; // dark text on the highlight
pub(crate) const TAG_BG: u32 = 0x9d86e933; // #tag pill background (translucent accent)
pub(crate) const SCROLLBAR: u32 = 0x6e6e6e66; // overlay thumb, idle (translucent grey)
pub(crate) const SCROLLBAR_HOVER: u32 = 0x9a9a9acc; // thumb while hovered or dragged

pub(crate) const IMAGE_MAX_H: f32 = 400.0; // cap rendered image height (px)
pub(crate) const IMAGE_PAD: f32 = 6.0; // vertical breathing room around an image (px)
pub(crate) const QUOTE_INDENT: f32 = 22.0; // left indent per blockquote nesting level (px)

/// Inset from the window edge to the text, on all four sides.
///
/// The element itself spans the *whole* window so the scrollbar can hug the edge
/// and run the full height, so the element owns this padding rather than getting
/// it from a wrapper div. 32px is what the previous `.p_8()` resolved to
/// (`rems(2.)` at gpui's default 16px rem), so the text lands on the same pixels.
pub(crate) const CONTENT_PAD: f32 = 32.0;

// Fonts are bundled (see `fonts/`) and registered at startup, so these families
// are always present and render identically without system fonts. GPUI's default
// `.SystemUIFont` doesn't resolve bold/italic faces, hence naming them directly.
//
// Public so a host can dress its own chrome to match the notes it is showing —
// see [`load_fonts`].
pub const UI_FONT: &str = "IBM Plex Sans";
pub const MONO_FONT: &str = "IBM Plex Mono";

// Font sizes (px).
pub(crate) const BODY_FONT: f32 = 18.0; // body text + non-heading lines
pub(crate) const CODE_FONT: f32 = 16.0; // fenced code blocks + frontmatter

static FONTS_LOADED: std::sync::Once = std::sync::Once::new();

/// Register the bundled IBM Plex faces with the text system, once per process.
///
/// Called when an editor is created, so the editor is self-sufficient. A host
/// that wants [`UI_FONT`] / [`MONO_FONT`] for its own chrome — so the app around
/// a note is set in the same face as the note — should call this at startup
/// instead of waiting for the first note to be opened. Idempotent.
pub fn load_fonts(cx: &App) {
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


// ---------------------------------------------------------------------------
// The editor view.
// ---------------------------------------------------------------------------

/// A live-preview markdown editor. Create with [`MarkdownEditor::new`] and use
/// it as a GPUI view (it implements [`Render`]).
pub struct MarkdownEditor {
    // The rope-backed buffer is the source of truth for text + undo/redo (delta
    // based, so no whole-document snapshots). `text` is a materialized cache of
    // its contents, refreshed after each mutation, that the parse/layout code
    // reads as `&str` (the parse is memoized by `buffer.version()`, so this is
    // materialized at most once per edit).
    pub(crate) buffer: Buffer,
    pub(crate) text: String,
    pub(crate) cursor: usize,         // byte offset into `text`
    pub(crate) anchor: Option<usize>, // selection anchor; selection is anchor..cursor
    // Preferred x for vertical motion ("goal column"): captured on the first of a
    // run of vertical moves and reused so up/down through short lines don't drift
    // and return to the same column. Cleared by any horizontal move, edit, or
    // click (see `set_cursor`), meaning "recompute from the caret next time".
    pub(crate) goal_x: Option<Pixels>,
    // IME composition ("marked text"): the byte range of the uncommitted preedit
    // while composing (dead keys, CJK). `None` when not composing. Rendered with
    // an underline so the in-progress text is visible.
    pub(crate) marked_range: Option<Range<usize>>,
    pub(crate) dragging: bool,
    pub(crate) scroll_top: Pixels,
    pub(crate) scroll_to_caret: bool,
    /// Height of the content box in the last painted frame. Lives here rather
    /// than on `Layout` because it is a property of the *frame*, not of the text:
    /// a height-only resize must not invalidate the layout (that would undo
    /// virtualization's win), yet page-scrolling and the scrollbar need the
    /// current value. Refreshed on every prepaint path via `finish`.
    pub(crate) viewport_height: Pixels,
    /// Height of the scrollbar track: the *whole* window, not the inset content
    /// box, so the thumb can reach the very top and bottom. Kept alongside
    /// `viewport_height` rather than derived from it, since the content box is
    /// clamped at zero for a window narrower/shorter than the padding.
    pub(crate) scroll_track: Pixels,
    /// While dragging the scrollbar thumb: the grab point's offset from the
    /// thumb's top, so the thumb follows the pointer 1:1.
    pub(crate) scrollbar_drag: Option<Pixels>,
    /// While drag-selecting with the pointer outside the text area: the pointer
    /// in *content-box* coordinates, so y is negative above the viewport and
    /// greater than `viewport_height` below it. A repeating task scrolls from
    /// this and re-hit-tests the selection end, which is what lets a selection
    /// grow past the visible page while the pointer is held still.
    autoscroll: Option<Point<Pixels>>,
    /// Generation of the running autoscroll loop, so a loop left over from an
    /// earlier drag exits instead of double-scrolling alongside the new one.
    /// (A cancel-on-drop `Task` can't be dropped from inside its own tick.)
    autoscroll_gen: u64,
    /// Pointer is over the scrollbar strip (or dragging it) — drawn brighter.
    pub(crate) scrollbar_hover: bool,
    pub(crate) layout: Option<Rc<Layout>>, // last painted layout, for movement + hit-testing
    pub(crate) focus_handle: FocusHandle,
    base_dir: Option<PathBuf>,  // note folder, for resolving relative image paths
    vault_dir: Option<PathBuf>, // vault root, for resolving ![[wikilink]] embeds
    image_cache: HashMap<PathBuf, Option<Arc<RenderImage>>>, // None = failed to load
    vault_index: Option<HashMap<String, PathBuf>>, // lazy basename -> image path map
    // Perf caches. The parse and the built layout are reused across frames when
    // their inputs are unchanged (keyed on `buffer.version()`), so a pure scroll
    // or selection change rebuilds nothing (see `prepaint`).
    parsed: Option<Rc<ParsedDoc>>,
    parsed_version: u64,
    pub(crate) layout_sig: Option<LayoutSig>,
    // The caret/selection the current layout was built with. Reveal depends on
    // these but the layout is not thrown away when they change — only the lines
    // whose reveal can differ are rebuilt (see `prepaint`'s patch path).
    pub(crate) layout_reveal: Option<Reveal>,
    pub(crate) shapes: ShapeCaches, // persistent shaped lines + table cells
    pub(crate) table_menu: Option<TableMenu>, // open table cell context menu, if any
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
        load_fonts(cx);
        bind_default_keys(cx);
        let text: String = content.into();
        let mut this = Self {
            buffer: Buffer::new(&text),
            text,
            cursor: 0,
            anchor: None,
            goal_x: None,
            marked_range: None,
            dragging: false,
            scroll_top: px(0.),
            scroll_to_caret: true,
            viewport_height: px(0.),
            scroll_track: px(0.),
            scrollbar_drag: None,
            scrollbar_hover: false,
            autoscroll: None,
            autoscroll_gen: 0,
            layout: None,
            focus_handle,
            base_dir,
            vault_dir,
            image_cache: HashMap::new(),
            vault_index: None,
            parsed: None,
            parsed_version: u64::MAX, // force first parse (buffer.version() starts at 0)
            layout_sig: None,
            layout_reveal: None,
            shapes: ShapeCaches::default(),
            table_menu: None,
        };
        this.maybe_drag_test(cx);
        this
    }

    /// Refresh the `text` cache from the buffer after a mutation.
    fn sync_text(&mut self) {
        self.text = self.buffer.text();
    }

    /// Apply a table structural edit (from the context menu) and place the caret
    /// in the affected cell.
    pub(crate) fn apply_table_op(&mut self, line: usize, col: usize, op: TableOp, cx: &mut Context<Self>) {
        self.table_menu = None;
        let Some((new_text, fl, fc)) = edit_table(&self.text, line, col, op) else {
            cx.notify();
            return;
        };
        // One undoable edit replacing the whole table region's document.
        self.buffer.replace(0..self.buffer.len_bytes(), &new_text, self.cursor, None);
        self.sync_text();
        // Place the caret at the focused cell's content start.
        let ls = line_start(&self.text, fl);
        let spans = table_cell_spans(line_str(&self.text, fl));
        self.cursor = spans.get(fc).map(|r| ls + r.start).unwrap_or(ls);
        self.anchor = None;
        self.goal_x = None;
        self.scroll_to_caret = true;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Parsed markdown for the current buffer, memoized until the text changes.
    /// Spans and markers are pre-bucketed per source line so building a line is
    /// O(line length) rather than O(total spans).
    pub(crate) fn ensure_parsed(&mut self) -> Rc<ParsedDoc> {
        if let Some(p) = &self.parsed {
            if self.parsed_version == self.buffer.version() {
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
        self.parsed_version = self.buffer.version();
        doc
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret's byte offset. Exposed for demos that drive the editor with
    /// synthetic keystrokes and check where the caret ends up (see the `keynav`
    /// example).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Place the caret at a byte offset (clamped to a char boundary). Handy for
    /// tests/demos that want to open with the caret somewhere specific.
    pub fn place_cursor(&mut self, offset: usize) {
        self.cursor = self.buffer.clamp_boundary(offset);
        self.anchor = None;
        self.goal_x = None;
    }

    pub fn set_text(
        &mut self,
        content: impl Into<String>,
        base_dir: Option<PathBuf>,
        vault_dir: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let text: String = content.into();
        self.buffer.set_text(&text);
        self.text = text;
        self.cursor = 0;
        self.anchor = None;
        self.goal_x = None;
        self.scroll_top = px(0.);
        self.scroll_to_caret = true;
        self.layout = None;
        self.layout_sig = None;
        self.layout_reveal = None;
        self.shapes = ShapeCaches::default();
        self.base_dir = base_dir;
        self.vault_dir = vault_dir;
        self.image_cache.clear();
        self.vault_index = None;
        cx.notify();
    }

    /// Resolve and decode an image reference, caching the result (including
    /// failures, so a bad path isn't retried every frame).
    pub(crate) fn image_for(&mut self, r: &ImageRef) -> Option<Arc<RenderImage>> {
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

    pub(crate) fn selection(&self) -> Option<Range<usize>> {
        match self.anchor {
            Some(a) if a != self.cursor => Some(a.min(self.cursor)..a.max(self.cursor)),
            _ => None,
        }
    }

    // ---- undo/redo (delegated to the buffer's delta history) ----

    fn undo(&mut self) {
        if let Some(c) = self.buffer.undo() {
            self.cursor = c;
            self.anchor = None;
            self.goal_x = None;
            self.sync_text();
        }
    }

    fn redo(&mut self) {
        if let Some(c) = self.buffer.redo() {
            self.cursor = c;
            self.anchor = None;
            self.goal_x = None;
            self.sync_text();
        }
    }

    // ---- editing ----

    /// Apply one edit through the buffer (records undo, bumps version), then
    /// refresh the text cache. The caret moves to just past the inserted text.
    fn edit(&mut self, range: Range<usize>, text: &str, coalesce: Option<u8>) {
        self.cursor = self.buffer.replace(range, text, self.cursor, coalesce);
        self.anchor = None;
        self.goal_x = None;
        self.sync_text();
    }

    fn insert(&mut self, s: &str) {
        // A single non-whitespace char coalesces into the current typing run;
        // whitespace, larger inserts, or replacing a selection start a fresh
        // undo group.
        let coalesce = (self.selection().is_none()
            && s.chars().count() == 1
            && !s.contains(char::is_whitespace))
        .then_some(1);
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.edit(range, s, coalesce);
    }

    /// Enter. On a list item this continues the list — a fresh marker on the
    /// next line, task boxes unchecked and ordered numbers advanced — and on an
    /// item left empty it removes the marker instead, so a list can be ended
    /// with the same key that extends it. Anywhere else it is a plain newline.
    fn insert_newline(&mut self) {
        // Replacing a selection is an ordinary edit: the caret's line is about
        // to be cut in half, so there is no item to carry forward.
        if self.selection().is_some() {
            self.insert("\n");
            return;
        }
        let line = offset_to_line_col(&self.text, self.cursor).0;
        let in_code = self.ensure_parsed().code[line].in_block;
        let (range, insert) = newline_edit(&self.text, self.cursor, in_code);
        self.edit(range, &insert, None);
    }

    fn backspace(&mut self) {
        if let Some(sel) = self.selection() {
            self.edit(sel, "", None);
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let prev = self.buffer.prev_boundary(self.cursor);
        self.edit(prev..self.cursor, "", Some(2));
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
            self.edit(sel, "", None);
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(s) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if s.is_empty() {
            return;
        }
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.edit(range, &s, None);
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
        self.goal_x = None;
    }

    pub(crate) fn set_cursor(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
        // A horizontal move / click drops the sticky goal column; vertical moves
        // restore it right after calling this (see `move_vertical`/`move_page`).
        self.goal_x = None;
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

    /// Move up/down one visual row using the last painted layout (wrap-aware),
    /// honoring the sticky goal column so repeated moves keep a stable x.
    fn move_vertical(&mut self, down: bool, shift: bool) {
        if let Some(layout) = self.layout.clone() {
            if let Some((x, y, lh)) = layout.caret(self.cursor) {
                let goal = self.goal_x.unwrap_or(x);
                let target_y = if down { y + lh + px(1.) } else { (y - px(1.)).max(px(0.)) };
                let new = layout.offset_at(goal, target_y);
                self.set_cursor(new, shift); // clears goal_x…
                self.goal_x = Some(goal); // …which we immediately re-sticky
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

    /// Thumb geometry for the current frame, or `None` when the document fits and
    /// no scrollbar is shown. Uses the layout's exact `total_height`, so the thumb
    /// is correctly sized even though most lines are never shaped.
    pub(crate) fn scrollbar(&self) -> Option<ScrollbarGeom> {
        let layout = self.layout.as_ref()?;
        scrollbar_geom(
            layout.total_height,
            self.viewport_height,
            self.scroll_top,
            self.scroll_track,
        )
    }

    /// Point drag-autoscroll at `pos` (the pointer, in content-box coordinates),
    /// or stop it with `None`. Starting it spawns a loop that keeps scrolling
    /// until the pointer comes back inside, the button is released, or the view
    /// goes away; calling it again while it runs only updates the target.
    pub(crate) fn set_autoscroll(&mut self, pos: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let start = pos.is_some() && self.autoscroll.is_none();
        self.autoscroll = pos;
        if !start {
            return;
        }
        self.autoscroll_gen = self.autoscroll_gen.wrapping_add(1);
        let generation = self.autoscroll_gen;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_TICK).await;
                let go = this
                    .update(cx, |e, cx| {
                        e.autoscroll_gen == generation && e.autoscroll_tick(cx)
                    })
                    .unwrap_or(false); // the view was dropped
                if !go {
                    break;
                }
            }
        })
        .detach();
    }

    /// Test hatch: with `SLIMNOTES_DRAG_TEST=1`, park a simulated text drag below
    /// the text area shortly after the window opens and log the scroll offset and
    /// selection as it runs. There is no mouse injection in this dev setup, so
    /// this is the only way to exercise autoscroll end to end — the tick loop,
    /// the repaint, and the hit-test that grows the selection — rather than just
    /// the [`drag_scroll_step`] ramp its unit tests cover.
    fn maybe_drag_test(&mut self, cx: &mut Context<Self>) {
        if std::env::var_os("SLIMNOTES_DRAG_TEST").is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            // Let the first frame land, so there is a layout to hit-test against.
            cx.background_executor().timer(Duration::from_millis(1200)).await;
            this.update(cx, |e, cx| {
                e.cursor = 0;
                e.anchor = Some(0);
                e.dragging = true;
                let below = point(px(40.), e.viewport_height + px(120.));
                e.set_autoscroll(Some(below), cx);
            })
            .ok();
            for _ in 0..8 {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                this.update(cx, |e, _| {
                    eprintln!(
                        "drag-test: scroll_top={} selection={}..{}",
                        f32::from(e.scroll_top),
                        e.anchor.unwrap_or(0),
                        e.cursor
                    );
                })
                .ok();
            }
            this.update(cx, |e, cx| {
                e.dragging = false;
                e.set_autoscroll(None, cx);
            })
            .ok();
        })
        .detach();
    }

    /// One autoscroll frame: scroll by the ramped step, then extend the selection
    /// to whatever the pointer now points at — the pointer hasn't moved, the text
    /// under it has. Returns whether to keep ticking.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(pos) = self.autoscroll else { return false };
        if !self.dragging {
            self.autoscroll = None; // button released without a mouse-up here
            return false;
        }
        let step = drag_scroll_step(pos.y, self.viewport_height);
        let max = max_scroll(self.total_height(), self.viewport_height);
        let scrolled = (self.scroll_top + step).clamp(px(0.), max);
        let cursor = match self.layout.clone() {
            Some(layout) => layout.offset_at(pos.x, pos.y + scrolled),
            None => self.cursor,
        };
        // Keep ticking even when nothing moved — at either end of the document
        // the pointer is still outside, and it may come back or the document may
        // grow. But don't repaint for a frame that would be identical.
        if scrolled != self.scroll_top || cursor != self.cursor {
            self.scroll_top = scrolled;
            self.cursor = cursor;
            self.goal_x = None;
            self.scroll_to_caret = false; // we are the ones choosing the offset
            cx.notify();
        }
        true
    }

    /// Total document height as of the last layout (0 before the first one).
    pub(crate) fn total_height(&self) -> Pixels {
        self.layout.as_ref().map(|l| l.total_height).unwrap_or(px(0.))
    }

    /// Move the caret ~one viewport up/down, keeping the goal column (PageUp/Down).
    fn move_page(&mut self, down: bool, shift: bool) {
        let Some(layout) = self.layout.clone() else { return };
        let Some((x, y, _lh)) = layout.caret(self.cursor) else { return };
        let goal = self.goal_x.unwrap_or(x);
        let vh = self.viewport_height;
        let target_y = if down { y + vh } else { (y - vh).max(px(0.)) };
        let new = layout.offset_at(goal, target_y);
        self.set_cursor(new, shift);
        self.goal_x = Some(goal);
    }

    /// Move left/right one character (table-gap aware), extending on shift.
    fn move_horizontal(&mut self, right: bool, shift: bool) {
        if right {
            if let Some(ch) = self.text[self.cursor..].chars().next() {
                let p = self.snap_table_h(self.cursor + ch.len_utf8(), true);
                self.set_cursor(p, shift);
            }
        } else if self.cursor > 0 {
            let p = self.snap_table_h(prev_boundary(&self.text, self.cursor), false);
            self.set_cursor(p, shift);
        }
    }

    /// Move left/right one word (Ctrl/Alt+Arrow), extending on shift.
    fn move_word(&mut self, right: bool, shift: bool) {
        let p = if right {
            word_right(&self.text, self.cursor)
        } else {
            word_left(&self.text, self.cursor)
        };
        self.set_cursor(p, shift);
    }

    /// Home: line (or first table cell) start; extending on shift.
    fn move_home(&mut self, shift: bool) {
        let (line, _) = offset_to_line_col(&self.text, self.cursor);
        // In a table row, Home goes to the first cell's content, not the `|`.
        let p = match self.table_row_cells(line) {
            Some((ls, _, spans)) if !spans.is_empty() => ls + spans[0].start,
            _ => line_start(&self.text, line),
        };
        self.set_cursor(p, shift);
    }

    /// End: line (or last table cell) end; extending on shift.
    fn move_end(&mut self, shift: bool) {
        let (line, _) = offset_to_line_col(&self.text, self.cursor);
        let p = match self.table_row_cells(line) {
            Some((ls, _, spans)) if !spans.is_empty() => ls + spans.last().unwrap().end,
            _ => line_start(&self.text, line) + line_str(&self.text, line).len(),
        };
        self.set_cursor(p, shift);
    }

    /// Forward delete: remove the selection, else the char after the caret.
    fn delete_forward(&mut self) {
        if let Some(sel) = self.selection() {
            self.edit(sel, "", None);
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.buffer.next_boundary(self.cursor);
        self.edit(self.cursor..next, "", Some(3));
    }

    /// If a content-space point falls on a rendered checkbox glyph, return the
    /// document offset of its toggle char (the byte between the brackets).
    pub(crate) fn checkbox_hit(&self, x: Pixels, content_y: Pixels) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let l = layout.line_at_y(content_y)?;
        let local_y = content_y - l.top;
        // `check_rect` yields nothing for an unshaped line, which has no glyph
        // positions — and is off-screen anyway, so it can't be clicked.
        for c in &l.checks {
            if l.check_rect(c).is_some_and(|r| r.contains(&point(x, local_y))) {
                return Some(c.inner_abs);
            }
        }
        None
    }

    pub(crate) fn toggle_checkbox(&mut self, inner_abs: usize) {
        if inner_abs >= self.text.len() {
            return;
        }
        let repl = if self.text.as_bytes()[inner_abs] == b' ' {
            "x"
        } else {
            " "
        };
        // Preserve the caret; toggling shouldn't move it.
        let caret = self.cursor;
        self.edit(inner_abs..inner_abs + 1, repl, None);
        self.cursor = caret.min(self.text.len());
    }

}

impl EventEmitter<EditorEvent> for MarkdownEditor {}

/// The platform text-input protocol: this is how printable characters actually
/// reach the buffer, and what makes IME composition (dead keys, CJK) work.
///
/// Every range in this trait is in **UTF-16 code units** (it mirrors macOS's
/// `NSTextInputClient`), while the editor works in byte offsets — so each one is
/// converted through the rope (see `Buffer::{byte_to_utf16, utf16_to_byte}`).
impl EntityInputHandler for MarkdownEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        adjusted.replace(self.range_to_utf16(&range));
        Some(self.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = self.selection().unwrap_or(self.cursor..self.cursor);
        Some(UTF16Selection {
            range: self.range_to_utf16(&sel),
            // The head is at the start when the caret sits before the anchor.
            reversed: self.anchor.is_some_and(|a| self.cursor < a),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let m = self.marked_range.clone()?;
        Some(self.range_to_utf16(&m))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    /// Commit text: replaces the given range (or the preedit, or the selection).
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.input_target(range_utf16);
        // A single non-whitespace char coalesces into the current typing run.
        let coalesce = (range.is_empty()
            && text.chars().count() == 1
            && !text.contains(char::is_whitespace))
        .then_some(1);
        self.edit(range, text, coalesce);
        self.marked_range = None;
        self.after_input(cx);
        cx.emit(EditorEvent::Changed);
    }

    /// Update the in-progress composition. The inserted text stays "marked"
    /// until the IME commits it via `replace_text_in_range`.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.input_target(range_utf16);
        let start = range.start;
        // Composition churn is one undo step, not one per keystroke.
        self.edit(range, new_text, None);
        if new_text.is_empty() {
            // Composition cancelled.
            self.marked_range = None;
            self.cursor = start.min(self.text.len());
        } else {
            let end = (start + new_text.len()).min(self.text.len());
            self.marked_range = Some(start..end);
            // The IME may ask for the caret to sit inside the preedit.
            self.cursor = match new_selected_range_utf16 {
                Some(sel) => {
                    let local = self.buffer.utf16_to_byte(self.buffer.byte_to_utf16(start) + sel.end);
                    local.clamp(start, end)
                }
                None => end,
            };
        }
        self.anchor = None;
        self.after_input(cx);
        cx.emit(EditorEvent::Changed);
    }

    /// Where to put the IME candidate window: the caret's rect, in window space.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let (x, y, lh) = self.layout.as_ref()?.caret(range.start)?;
        Some(Bounds {
            origin: element_bounds.origin + point(x, y - self.scroll_top),
            size: size(px(2.), lh),
        })
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let offset = layout.offset_at(point.x, point.y + self.scroll_top);
        Some(self.buffer.byte_to_utf16(offset))
    }
}

impl MarkdownEditor {
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.buffer.byte_to_utf16(range.start)..self.buffer.byte_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.buffer.utf16_to_byte(range.start)..self.buffer.utf16_to_byte(range.end)
    }

    /// The byte range an IME edit applies to: the range it named, else the
    /// current preedit, else the selection (an empty range = insert at caret).
    fn input_target(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or_else(|| self.marked_range.clone())
            .or_else(|| self.selection())
            .unwrap_or(self.cursor..self.cursor)
    }
}

impl MarkdownEditor {
    /// The table cell context menu, anchored at the (window-space) click point.
    /// Each item is a real interactive element that applies its own op, so the
    /// menu's geometry lives in exactly one place.
    fn render_table_menu(&self, m: TableMenu, cx: &mut Context<Self>) -> impl IntoElement {
        let items = TABLE_MENU_ITEMS.iter().enumerate().map(|(i, (label, op))| {
            let danger = matches!(op, TableOp::DeleteRow | TableOp::DeleteColumn);
            let op = *op;
            div()
                .id(("table-menu-item", i))
                .h(px(TABLE_MENU_ITEM_H))
                .px_3()
                .flex()
                .items_center()
                .text_size(px(13.))
                .text_color(if danger { rgb(0xdd8a8a) } else { rgb(TEXT) })
                .hover(|s| s.bg(rgb(0x3a3a3a)))
                .cursor_pointer()
                // Mouse-*down*, not click: the element's own mouse-down handler
                // dismisses the menu, which would unmount this item before a
                // click's mouse-up could resolve. Both fire on the same event, so
                // the op applies regardless of which handler runs first.
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _window, cx| {
                        this.apply_table_op(m.line, m.col, op, cx);
                    }),
                )
                .child(SharedString::from(*label))
        });
        deferred(
            anchored().position(m.pos).child(
                div()
                    .w(px(TABLE_MENU_W))
                    .py(px(TABLE_MENU_VPAD))
                    // On the whole panel, not just the items: otherwise the
                    // text's I-beam would show through the padding strips at
                    // the top and bottom of the menu.
                    .cursor_pointer()
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
        // Printable characters arrive through the platform input handler (see the
        // `EntityInputHandler` impl), not a key listener, so IME composition works;
        // everything else is a bound action.
        let root = div()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus_handle)
            .key_context(input::CONTEXT);
        // No padding here: the element takes the full window rect so the overlay
        // scrollbar can sit at the very edge, and insets the text itself by
        // `CONTENT_PAD`.
        self.on_input_actions(root, cx)
            .child(EditorElement {
                editor: cx.entity(),
            })
            .children(
                self.table_menu
                    .map(|m| self.render_table_menu(m, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Font, FontStyle, FontWeight};

    /// A layout of unshaped lines with the given heights, for exercising the
    /// geometry lookups without a text system.
    fn geom_layout(heights: &[f32]) -> Layout {
        let mut lines = Vec::new();
        let mut top = px(0.);
        let mut start = 0usize;
        for (i, &h) in heights.iter().enumerate() {
            lines.push(VisualLine {
                start,
                source: format!("line {i}"),
                segs: Vec::new(),
                checks: Vec::new(),
                code_bg: false,
                hr: false,
                left_inset: px(0.),
                quote_bars: 0,
                table: None,
                image: None,
                wrapped: None,
                shape_in: None,
                line_height: px(27.),
                top,
                height: px(h),
                hang: px(0.),
            });
            top += px(h);
            start += 8; // "line N" + newline; only the ordering matters
        }
        Layout {
            lines,
            total_height: top,
            links: Vec::new(),
        }
    }

    #[test]
    fn line_index_at_y_binary_search() {
        // Rows of 27, 54 (a wrapped line), 27.
        let l = geom_layout(&[27., 54., 27.]);
        assert_eq!(l.line_index_at_y(px(0.)), Some(0));
        assert_eq!(l.line_index_at_y(px(26.9)), Some(0));
        assert_eq!(l.line_index_at_y(px(27.)), Some(1)); // exactly on a boundary
        assert_eq!(l.line_index_at_y(px(80.)), Some(1));
        assert_eq!(l.line_index_at_y(px(81.)), Some(2));
        // Past the end clamps to the last line rather than falling off.
        assert_eq!(l.line_index_at_y(px(10_000.)), Some(2));
        // A negative y (possible mid-scroll) clamps to the first.
        assert_eq!(l.line_index_at_y(px(-50.)), Some(0));
        // Degenerate documents.
        assert_eq!(geom_layout(&[]).line_index_at_y(px(5.)), None);
        assert_eq!(geom_layout(&[27.]).line_index_at_y(px(500.)), Some(0));
    }

    #[test]
    fn line_index_at_offset_binary_search() {
        let l = geom_layout(&[27., 27., 27.]); // starts 0, 8, 16
        assert_eq!(l.line_index_at_offset(0), Some(0));
        assert_eq!(l.line_index_at_offset(7), Some(0));
        assert_eq!(l.line_index_at_offset(8), Some(1));
        assert_eq!(l.line_index_at_offset(16), Some(2));
        assert_eq!(l.line_index_at_offset(usize::MAX), Some(2));
        assert_eq!(geom_layout(&[]).line_index_at_offset(0), None);
    }

    #[test]
    fn content_rect_insets_the_window_on_all_sides() {
        use gpui::{size, Bounds};
        let pad = CONTENT_PAD;
        let b = Bounds {
            origin: gpui::point(px(0.), px(0.)),
            size: size(px(900.), px(700.)),
        };
        let c = content_rect(b);
        assert_eq!(f32::from(c.origin.x), pad);
        assert_eq!(f32::from(c.origin.y), pad);
        assert_eq!(f32::from(c.size.width), 900. - pad * 2.);
        assert_eq!(f32::from(c.size.height), 700. - pad * 2.);
        // The inset is relative to the element's origin, not the screen's.
        let b = Bounds {
            origin: gpui::point(px(10.), px(20.)),
            size: size(px(900.), px(700.)),
        };
        let c = content_rect(b);
        assert_eq!(f32::from(c.origin.x), 10. + pad);
        assert_eq!(f32::from(c.origin.y), 20. + pad);
        // A window smaller than the padding clamps to zero rather than going
        // negative (which would make the wrap width and viewport nonsense).
        let b = Bounds {
            origin: gpui::point(px(0.), px(0.)),
            size: size(px(10.), px(10.)),
        };
        let c = content_rect(b);
        assert_eq!(f32::from(c.size.width), 0.);
        assert_eq!(f32::from(c.size.height), 0.);
    }

    #[test]
    fn shape_window_covers_viewport_plus_overscan() {
        // 100 lines of 27px each => 2700px tall; viewport 270px (10 lines).
        let l = geom_layout(&[27.; 100]);
        let viewport = px(270.);

        // At the top, the window starts at line 0 and reaches at least the
        // viewport plus a screen of overscan below.
        let w = shape_window(&l.lines, px(0.), viewport);
        assert_eq!(w.start, 0);
        assert!(w.end >= 20, "expected viewport+overscan, got {w:?}");

        // Scrolled into the middle: the window brackets the visible lines.
        let w = shape_window(&l.lines, px(1350.), viewport); // lines 50..60 visible
        assert!(w.start <= 50 && w.end > 60, "{w:?}");
        assert!(w.start >= 39, "overscan should stay bounded, got {w:?}");

        // At the bottom it clamps to the line count (no out-of-range end).
        let w = shape_window(&l.lines, px(2430.), viewport);
        assert!(w.end <= l.lines.len(), "{w:?}");
        assert!(w.contains(&99), "last line must be shaped: {w:?}");

        // An empty document yields an empty, in-range window.
        let w = shape_window(&geom_layout(&[]).lines, px(0.), viewport);
        assert!(w.start == 0 && w.end == 0);
    }

    #[test]
    fn scroll_is_resolved_before_choosing_the_shape_window() {
        // 100 lines x 27px = 2700px; viewport 270px => max scroll 2430.
        let l = geom_layout(&[27.; 100]);
        let viewport = px(270.);
        let max_scroll = px(2430.);

        // Clamping: a scroll past the end settles at max_scroll, not beyond.
        assert_eq!(resolve_scroll(&l, px(9999.), false, 0, viewport), max_scroll);
        assert_eq!(resolve_scroll(&l, px(-100.), false, 0, viewport), px(0.));
        // No caret follow requested => the offset passes through untouched.
        assert_eq!(resolve_scroll(&l, px(500.), false, 0, viewport), px(500.));

        // Caret follow scrolls just far enough to reveal the caret's line. Line
        // starts step by 8 in the fixture, so line 99 starts at byte 792.
        let s = resolve_scroll(&l, px(0.), true, 792, viewport);
        assert_eq!(s, max_scroll, "caret on the last line must scroll to the end");

        // The regression: the window must be derived from the RESOLVED scroll.
        // A pre-clamp offset of 9999 would pick a window past the document and
        // leave the lines actually on screen unshaped (they then paint blank).
        let naive = shape_window(&l.lines, px(9999.), viewport);
        let resolved = shape_window(&l.lines, resolve_scroll(&l, px(9999.), false, 0, viewport), viewport);
        assert!(
            resolved.contains(&99),
            "resolved window must cover the last line: {resolved:?}"
        );
        // The last visible line at max scroll is 99; the naive window starts past
        // everything, which is exactly how the bottom of the document went blank.
        assert!(naive.start > resolved.start, "naive={naive:?} resolved={resolved:?}");
        for li in 90..100 {
            assert!(resolved.contains(&li), "line {li} must be shaped at the bottom");
        }
    }

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
    fn scanners_are_context_aware() {
        let has = |text: &str, attr: Attr| {
            let (spans, _m, _l) = parse(text);
            spans.iter().any(|s| s.attr == attr)
        };
        // Inline code shields ==, #tag, and [[ ]] from the hand scanners.
        assert!(!has("`x ==y==`", Attr::Highlight), "== in code");
        assert!(!has("`#tag`", Attr::Tag), "#tag in code");
        assert!(!has("`[[note]]`", Attr::Link), "[[ ]] in code");
        // Backslash-escaped markers stay literal.
        assert!(!has(r"\==nope==", Attr::Highlight), "escaped ==");
        // Flanking: spaced `==` in prose is not a highlight.
        assert!(!has("2 == 2 and 3 == 3", Attr::Highlight), "prose ==");
        // The real features still work.
        assert!(has("this ==is== marked", Attr::Highlight));
        assert!(has("a #tag here", Attr::Tag));
        assert!(has("see [[Some Note]] now", Attr::Link));
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

    /// Build a standalone cell the way the element does: parse the enclosing
    /// line, then hand `build_cell` the spans/markers covering the cell.
    fn cell_of(cell: &str, weight: FontWeight, cursor: usize) -> (String, Vec<gpui::TextRun>) {
        let (spans, markers, _l) = parse(cell);
        let (spans, markers) = cell_spans(&spans, &markers, 0..cell.len());
        let (display, runs, _segs) =
            build_cell(cell, 0, &spans, &markers, &test_font(), weight, cursor, None);
        (display, runs)
    }

    #[test]
    fn table_cell_renders_inline_markdown() {
        // Caret off the cell (large offset): markers concealed like body text.
        let far = 10_000;
        // A cell with an inline code span: backticks concealed, code background.
        let (display, runs) = cell_of("`postgres`", FontWeight::NORMAL, far);
        assert_eq!(display, "postgres");
        assert!(
            runs.iter().any(|r| r.background_color.is_some()),
            "code cell should have a background run"
        );
        // A header cell keeps its base weight bold while styling its content.
        let (disp, runs) = cell_of("**Role**", FontWeight::BOLD, far);
        assert_eq!(disp, "Role");
        assert!(runs.iter().all(|r| r.font.weight == FontWeight::BOLD));
        // The caret inside the cell reveals that cell's markers.
        let (disp, _runs) = cell_of("`postgres`", FontWeight::NORMAL, 3);
        assert_eq!(disp, "`postgres`");
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
    fn table_cell_selection_covers_the_whole_cell_or_part_of_it() {
        // A cell holding document bytes 10..20.
        let sel = |a: usize, b: usize| cell_selection(10, 20, Some((a, b)));
        // Untouched: entirely before, entirely after, or abutting either edge.
        assert_eq!(sel(0, 5), None);
        assert_eq!(sel(0, 10), None);
        assert_eq!(sel(20, 30), None);
        assert_eq!(cell_selection(10, 20, None), None);
        // Exactly the cell, and any selection that swallows it (the common case:
        // a drag across several rows) fills the box.
        assert_eq!(sel(10, 20), Some(CellSel::Whole));
        assert_eq!(sel(0, 999), Some(CellSel::Whole));
        // Partial covers are reported cell-local, ready for `src_to_disp`.
        assert_eq!(sel(12, 15), Some(CellSel::Part(2..5)));
        assert_eq!(sel(0, 13), Some(CellSel::Part(0..3)));
        assert_eq!(sel(17, 99), Some(CellSel::Part(7..10)));
    }

    #[test]
    fn empty_table_cell_still_selects() {
        // A blank cell has no text to highlight, but a selection running through
        // the row must not leave a hole in it.
        assert_eq!(cell_selection(10, 10, Some((0, 99))), Some(CellSel::Whole));
        assert_eq!(cell_selection(10, 10, Some((10, 20))), Some(CellSel::Whole));
        // Merely ending at the cell doesn't select it, and neither does starting
        // after it.
        assert_eq!(cell_selection(10, 10, Some((0, 10))), None);
        assert_eq!(cell_selection(10, 10, Some((11, 20))), None);
    }

    #[test]
    fn a_caret_in_a_cell_is_not_a_selection() {
        // `selection()` never yields a collapsed range, but the helper must not
        // depend on that: an empty range would otherwise paint a stray sliver.
        assert_eq!(cell_selection(10, 20, Some((15, 15))), None);
        assert_eq!(cell_selection(10, 10, Some((10, 10))), None);
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
        assert!(!hl.contains_key(&0) && !hl.contains_key(&2));
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
    fn word_motions_scan_boundaries() {
        let text = "foo bar_2  baz.qux";
        // Right: from start, stop at the end of each word run.
        assert_eq!(word_right(text, 0), 3); // end of "foo"
        assert_eq!(word_right(text, 3), 9); // skip space, end of "bar_2"
        assert_eq!(word_right(text, 9), 14); // skip 2 spaces, end of "baz"
        assert_eq!(word_right(text, 14), 15); // the lone '.'
        assert_eq!(word_right(text, 15), 18); // end of "qux"
        assert_eq!(word_right(text, 18), 18); // clamp at end
        // Left: mirror image.
        assert_eq!(word_left(text, 18), 15); // start of "qux"
        assert_eq!(word_left(text, 15), 14); // the '.'
        assert_eq!(word_left(text, 14), 11); // start of "baz" (two spaces at 9,10)
        assert_eq!(word_left(text, 10), 4); // skip spaces, start of "bar_2"
        assert_eq!(word_left(text, 4), 0); // start of "foo"
        assert_eq!(word_left(text, 0), 0); // clamp at start
    }

    #[test]
    fn word_motions_cross_newlines_and_multibyte() {
        let text = "ab\ncdé fg";
        // Whitespace (incl. '\n') is skipped like any other.
        // From end of "ab" over '\n' to end of "cdé" — 'é' spans bytes 5..7.
        assert_eq!(word_right(text, 2), 7);
        assert_eq!(word_left(text, 6), 3); // back to start of "cdé"
        // Never lands inside the 2-byte 'é'.
        assert!(text.is_char_boundary(word_right(text, 3)));
        assert!(text.is_char_boundary(word_left(text, 7)));
    }

    /// Press Enter in `before`, where `|` marks the caret, and return the
    /// document with the caret marked the same way — so each case reads as the
    /// keystroke it describes.
    fn enter(before: &str) -> String {
        enter_in_code(before, false)
    }

    fn enter_in_code(before: &str, in_code: bool) -> String {
        let cursor = before.find('|').expect("the caret must be marked with |");
        let text = before.replacen('|', "", 1);
        let (range, insert) = newline_edit(&text, cursor, in_code);
        let mut out = text;
        // The caret lands past the inserted text, as `Buffer::replace` reports.
        let caret = range.start + insert.len();
        out.replace_range(range, &insert);
        out.insert(caret, '|');
        out
    }

    #[test]
    fn enter_continues_a_list() {
        assert_eq!(enter("- one|"), "- one\n- |");
        assert_eq!(enter("* one|"), "* one\n* |");
        assert_eq!(enter("+ one|"), "+ one\n+ |");
        // Mid-item: the tail moves to the new item.
        assert_eq!(enter("- one|two"), "- one\n- |two");
        // Indentation and any alignment spaces are repeated verbatim.
        assert_eq!(enter("  - a|"), "  - a\n  - |");
        assert_eq!(enter("-   a|"), "-   a\n-   |");
        // A multi-byte char right before the caret is not split.
        assert_eq!(enter("- é|"), "- é\n- |");
    }

    #[test]
    fn enter_continues_a_task_list_unchecked() {
        assert_eq!(enter("- [ ] todo|"), "- [ ] todo\n- [ ] |");
        // A done item does not hand its checkmark to the next one.
        assert_eq!(enter("- [x] done|"), "- [x] done\n- [ ] |");
        assert_eq!(enter("1. [x] done|"), "1. [x] done\n2. [ ] |");
        // A bracket run that isn't a task box is ordinary content. This used to
        // panic: the box test sliced `[..]` without checking char boundaries.
        assert_eq!(enter("- é] x|"), "- é] x\n- |");
    }

    #[test]
    fn enter_advances_ordered_numbering() {
        assert_eq!(enter("3. c|"), "3. c\n4. |");
        assert_eq!(enter("1) a|"), "1) a\n2) |");
        assert_eq!(enter("  9. i|"), "  9. i\n  10. |");
        // Numbering is not restarted from a number we can't parse.
        let huge = "1".repeat(40);
        assert_eq!(enter(&format!("{huge}. x|")), format!("{huge}. x\n{huge}. |"));
    }

    #[test]
    fn a_second_enter_removes_the_empty_item() {
        // The first press left `- ` behind; the second takes it away, leaving a
        // blank line and no markup to delete by hand.
        assert_eq!(enter("- one\n- |"), "- one\n|");
        assert_eq!(enter("- [ ] |"), "|");
        assert_eq!(enter("2. |"), "|");
        // Indentation goes with it, and so does invisible trailing whitespace.
        assert_eq!(enter("  - |"), "|");
        assert_eq!(enter("-   |"), "|");
        // A third press is then an ordinary newline.
        assert_eq!(enter("- one\n|"), "- one\n\n|");
    }

    #[test]
    fn enter_is_plain_where_there_is_no_item_to_continue() {
        assert_eq!(enter("hello|"), "hello\n|");
        // A marker needs its space before it is a list.
        assert_eq!(enter("-|"), "-\n|");
        assert_eq!(enter("1.|"), "1.\n|");
        // Caret inside the marker: the item moves down whole, un-duplicated.
        assert_eq!(enter("|- one"), "\n|- one");
        assert_eq!(enter("-| one"), "-\n| one");
        // ...but at the content start it splits, leaving an empty item above.
        assert_eq!(enter("- |one"), "- \n- |one");
        // Inside a fenced block a leading `-` is code, not a list.
        assert_eq!(enter_in_code("- one|", true), "- one\n|");
        assert_eq!(enter_in_code("- |", true), "- \n|");
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

    /// Run `dirty_reveal_lines` over `text` for a caret/selection move.
    fn dirty_for(
        text: &str,
        old: (usize, Option<(usize, usize)>),
        new: (usize, Option<(usize, usize)>),
    ) -> Option<Vec<usize>> {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut line_off = Vec::new();
        let mut acc = 0;
        for l in &lines {
            line_off.push(acc);
            acc += l.len() + 1;
        }
        let code = classify_code(text);
        let tables = table_blocks(&lines);
        dirty_reveal_lines(
            &lines,
            &line_off,
            &code,
            &tables,
            Reveal { cursor: old.0, sel: old.1 },
            Reveal { cursor: new.0, sel: new.1 },
        )
    }

    /// A layout where exactly `shaped` are shaped and the rest are deferred
    /// (i.e. have shaping inputs waiting), which is the state virtualization
    /// leaves behind.
    fn shape_state(n: usize, shaped: &[usize]) -> Layout {
        let mut layout = geom_layout(&vec![27.; n]);
        for (i, l) in layout.lines.iter_mut().enumerate() {
            if shaped.contains(&i) {
                l.wrapped = Some(gpui::WrappedLine::default());
            } else {
                l.shape_in = Some(ShapeIn {
                    display: SharedString::from(""),
                    runs: Vec::new(),
                    font_size: px(18.),
                    width: px(500.),
                });
            }
        }
        layout
    }

    #[test]
    fn a_line_crossing_the_padding_counts_as_on_screen() {
        // An element 0..700 whose text box is inset by CONTENT_PAD: 32..668.
        // Text is not clipped to the text box, so the edges passed here are the
        // element's — a line inside the padding bands IS visible and must paint.
        let (top, bottom) = (px(0.), px(700.));
        let lh = px(27.);
        // Fully inside the bottom padding band (below the text box, above the
        // window edge): visible. Culling this was the bug.
        assert!(line_on_screen(px(675.), lh, top, bottom));
        // Fully inside the top band (above the text box, below the window top).
        assert!(line_on_screen(px(5.), lh, top, bottom));
        // Straddling either edge of the window.
        assert!(line_on_screen(px(-10.), lh, top, bottom));
        assert!(line_on_screen(px(695.), lh, top, bottom));
        // Genuinely off screen, above and below.
        assert!(!line_on_screen(px(-40.), lh, top, bottom));
        assert!(!line_on_screen(px(701.), lh, top, bottom));
        // A tall (wrapped) line whose middle covers the whole window.
        assert!(line_on_screen(px(-500.), px(2000.), top, bottom));
    }

    #[test]
    fn missing_shapes_sees_the_gap_a_range_hull_would_hide() {
        // The state after jumping the scrollbar far down while the caret stays
        // at the top: line 0 (the caret's) and 20..25 (the viewport) are shaped,
        // everything between is still deferred.
        let l = shape_state(30, &[0, 20, 21, 22, 23, 24]);
        // The viewport itself is ready...
        assert!(!missing_shapes(&l.lines, 20..25));
        assert!(!missing_shapes(&l.lines, 0..1));
        // ...but the lines between the caret and the viewport are NOT, even
        // though a "shaped 0..25" hull spanning both would claim they are. That
        // claim is what made a screenful paint blank after scrolling back up.
        assert!(missing_shapes(&l.lines, 5..10));
        assert!(missing_shapes(&l.lines, 0..25));
        // Out-of-range indices are clamped, never panic.
        assert!(!missing_shapes(&l.lines, 100..200));
        assert!(missing_shapes(&l.lines, 25..999));
    }

    #[test]
    fn missing_shapes_ignores_lines_that_cannot_be_shaped() {
        // No shaping inputs (tables, images, rules, blank lines) — nothing to
        // fill in, so they must not keep triggering re-shaping every frame.
        let l = geom_layout(&[27., 27., 27.]);
        assert!(!missing_shapes(&l.lines, 0..3));
    }

    #[test]
    fn dirty_lines_cover_caret_moves() {
        // Four standalone paragraphs (blank-line separated), so no paragraph
        // expansion bleeds between them.
        let text = "aaa\n\nbbb\n\nccc\n\nddd";
        let l = |i: usize| text.split('\n').take(i).map(|s| s.len() + 1).sum::<usize>();
        // Caret moving between two paragraphs dirties only those two lines.
        assert_eq!(dirty_for(text, (l(0), None), (l(2), None)).unwrap(), vec![0, 2]);
        // Staying on the same line dirties just it.
        assert_eq!(dirty_for(text, (l(4), None), (l(4) + 1, None)).unwrap(), vec![4]);
    }

    #[test]
    fn dirty_lines_cover_selection_span() {
        let text = "aaa\n\nbbb\n\nccc\n\nddd";
        let l = |i: usize| text.split('\n').take(i).map(|s| s.len() + 1).sum::<usize>();
        // A selection dirties every line it covers, plus the new caret line.
        let d = dirty_for(text, (l(0), None), (l(4), Some((l(0), l(4) + 3)))).unwrap();
        assert_eq!(d, vec![0, 1, 2, 3, 4]);
        // Collapsing that selection dirties the lines it used to cover.
        let d = dirty_for(text, (l(4), Some((l(0), l(4) + 3))), (l(4), None)).unwrap();
        assert_eq!(d, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn dirty_lines_expand_to_paragraph() {
        // An inline span can cross a soft break (`*foo\nbar*`), so entering a
        // paragraph must dirty the whole paragraph, not just the caret's line.
        let text = "*foo\nbar*\n\nother";
        let d = dirty_for(text, (text.len(), None), (0, None)).unwrap();
        assert!(d.contains(&0) && d.contains(&1), "whole paragraph dirty: {d:?}");
    }

    #[test]
    fn dirty_lines_expand_to_table_and_code_block() {
        // A table's column widths depend on every row, so any caret move inside
        // it dirties the whole block.
        let text = "| a | b |\n| --- | --- |\n| 1 | 2 |\n\ntail";
        let d = dirty_for(text, (text.len(), None), (2, None)).unwrap();
        assert!(d.contains(&0) && d.contains(&1) && d.contains(&2));
        // Entering a fenced block dirties it (its ``` fences reveal).
        let text = "text\n\n```rust\nfn x() {}\n```\n\ntail";
        let d = dirty_for(text, (0, None), (text.find("fn x").unwrap(), None)).unwrap();
        assert!(d.contains(&2) && d.contains(&3) && d.contains(&4), "{d:?}");
    }

    #[test]
    fn dirty_lines_bail_out_when_huge() {
        // A selection over more lines than the patch threshold falls back to a
        // full rebuild (None) rather than patching thousands of lines.
        let text = "x\n".repeat(MAX_DIRTY_LINES + 50);
        assert!(dirty_for(&text, (0, None), (text.len(), Some((0, text.len())))).is_none());
    }

    /// The safety property behind the patch path: any line whose *rendered
    /// output* changes with the reveal state must be reported dirty, or the
    /// layout would keep a stale line. Checked by building every line under both
    /// reveal states and comparing — no window/shaping needed.
    #[test]
    fn dirty_lines_never_miss_a_changed_line() {
        let text = "# Head *a*\n\nplain `code` and **bold**\nsecond *line\nspan* here\n\n\
                    | `a` | **b** |\n| --- | --- |\n| c | d |\n\n\
                    ```rust\nlet x = 1;\n```\n\n> quote **q**\n\n- [ ] task ==hi==\n\ntail #tag";
        let lines: Vec<&str> = text.split('\n').collect();
        let mut line_off = Vec::new();
        let mut acc = 0;
        for l in &lines {
            line_off.push(acc);
            acc += l.len() + 1;
        }
        let (spans, markers, _l) = parse(text);
        let base = test_font();
        // Render one line's display text under a given reveal state.
        let render = |li: usize, rv: Reveal| {
            let (ls, le) = (line_off[li], line_off[li] + lines[li].len());
            let ls_spans: Vec<Span> =
                spans.iter().filter(|s| s.start < le && s.end > ls).cloned().collect();
            let ls_marks: Vec<(usize, usize)> =
                markers.iter().copied().filter(|m| m.1 > ls && m.0 < le).collect();
            let ops = line_ops(lines[li], ls, &ls_marks, &ls_spans, rv.cursor, rv.sel);
            let dim = dim_ranges(lines[li], ls, &ls_marks, &ls_spans, rv.cursor, rv.sel);
            let (display, runs, _s, _c) = build_line(
                lines[li], ls, &ops, &ls_spans, &base, &dim, FontWeight::NORMAL, false,
            );
            // Fold the dim styling in too, so a reveal that only changes colors
            // still counts as a difference.
            (display, runs.iter().map(|r| (r.len, r.color)).collect::<Vec<_>>())
        };

        // Sweep the caret across every line start, plus a few selections.
        let states: Vec<Reveal> = line_off
            .iter()
            .map(|&o| Reveal { cursor: o, sel: None })
            .chain([
                Reveal { cursor: 0, sel: Some((0, text.len())) },
                Reveal { cursor: 12, sel: Some((12, 30)) },
            ])
            .collect();
        for old in &states {
            for new in &states {
                let dirty = dirty_reveal_lines(
                    &lines,
                    &line_off,
                    &classify_code(text),
                    &table_blocks(&lines),
                    *old,
                    *new,
                );
                let Some(dirty) = dirty else { continue }; // full rebuild: always safe
                for (li, src) in lines.iter().enumerate() {
                    if render(li, *old) != render(li, *new) {
                        assert!(
                            dirty.contains(&li),
                            "line {li} ({:?}) changes between {:?}/{:?} but wasn't dirty: {dirty:?}",
                            src,
                            (old.cursor, old.sel),
                            (new.cursor, new.sel),
                        );
                    }
                }
            }
        }
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
