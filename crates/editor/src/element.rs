//! The custom GPUI element: shapes + wraps lines, paints text/selection/caret
//! with scrolling, and routes mouse + wheel events back to the editor entity.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    fill, point, px, quad, relative, rgb, rgba, size, transparent_black, App, BorderStyle, Bounds,
    ContentMask, Corners, CursorStyle, DispatchPhase, Element, ElementId, ElementInputHandler,
    Entity, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    ScrollDelta, ScrollWheelEvent, SharedString, Style, TextAlign, TextRun, WrappedLine, Window,
};

use crate::*;

pub(crate) struct EditorElement {
    pub(crate) editor: Entity<MarkdownEditor>,
}

/// The inputs that force a *full* layout rebuild. Deliberately excludes the
/// caret and selection: those only change marker reveal, which is patched
/// line-by-line instead of rebuilding the document (see `dirty_reveal_lines`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutSig {
    version: u64, // buffer.version()
    width: u32,   // wrap width, f32 bits
}

/// Cache key for a shaped line. A `WrappedLine` is fully determined by its
/// display text, font size, wrap width and runs (the runs also bake in colors /
/// decorations, so those are folded into `runs`). Same key → identical shaping.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ShapeKey {
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

/// The editor's persistent shaping cache: soft-wrapped lines, keyed by text,
/// size, wrap width and run styling — body lines and table cells alike.
#[derive(Default)]
pub(crate) struct ShapeCaches {
    wrapped: HashMap<ShapeKey, WrappedLine>,
}

/// Serves shaped lines for one build, from cache where possible.
///
/// On a full rebuild, hits move out of `prev` into `cur` and whatever is left in
/// `prev` is dropped, bounding the cache to the lines actually used. On a patch
/// only a couple of lines are rebuilt, so nothing may be evicted — every other
/// line in the reused layout still needs its entry.
struct Shaper {
    prev: Option<ShapeCaches>,
    cur: ShapeCaches,
}

impl Shaper {
    fn rebuilding(prev: ShapeCaches) -> Self {
        Self { prev: Some(prev), cur: ShapeCaches::default() }
    }

    fn patching(cur: ShapeCaches) -> Self {
        Self { prev: None, cur }
    }

    fn finish(self) -> ShapeCaches {
        self.cur
    }

    fn key(display: &SharedString, font_size: Pixels, width: Pixels, runs: &[TextRun]) -> ShapeKey {
        ShapeKey {
            text: display.clone(),
            font_size: f32::from(font_size).to_bits(),
            width: f32::from(width).to_bits(),
            runs: runs_digest(runs),
        }
    }

    /// A soft-wrapped body line.
    fn wrapped(
        &mut self,
        window: &mut Window,
        display: &SharedString,
        font_size: Pixels,
        runs: &[TextRun],
        width: Pixels,
    ) -> WrappedLine {
        let shape_width = width.max(px(40.));
        let key = Self::key(display, font_size, shape_width, runs);
        if let Some(hit) = self
            .prev
            .as_mut()
            .and_then(|p| p.wrapped.remove(&key))
            .or_else(|| self.cur.wrapped.get(&key).cloned())
        {
            self.cur.wrapped.insert(key, hit.clone());
            return hit;
        }
        // A shaping miss must never unwind through GPUI's render loop (that would
        // crash the window mid-edit); fall back to an empty line.
        let shaped = window
            .text_system()
            .shape_text(display.clone(), font_size, runs, Some(shape_width), None)
            .ok()
            .and_then(|v| v.into_iter().next())
            .unwrap_or_default();
        self.cur.wrapped.insert(key, shaped.clone());
        shaped
    }

}

/// Proves, without shaping, that a line fits on a single row.
///
/// Sums each character's advance in its own run's font. Shaping only ever makes
/// a line *narrower* than that sum (kerning and ligatures subtract), so the sum
/// is an upper bound: if it fits the wrap width, the shaped line cannot wrap, and
/// the height is exactly one row. Measured on IBM Plex: shaped/bound peaks at
/// 0.999 for text whose glyphs are all present.
///
/// The bound is only trustworthy when every glyph resolves in its run's font —
/// a missing glyph (emoji, CJK) is drawn from a fallback face whose advance we
/// haven't measured, and would *underestimate* (measured: 1.10x for emoji, 1.76x
/// for CJK). `advance` reporting an error for such a character is exactly the
/// signal, so those lines fall back to shaping.
struct WidthBounder {
    /// Per (font, size) character advances, memoized like gpui's own wrapper.
    cache: HashMap<(gpui::FontId, u32), HashMap<char, Pixels>>,
}

impl WidthBounder {
    fn new() -> Self {
        Self { cache: HashMap::new() }
    }

    /// `Some(width)` if every glyph resolved; `None` if any did not (caller must
    /// shape instead of trusting a bound).
    fn bound(
        &mut self,
        cx: &App,
        text: &str,
        runs: &[TextRun],
        font_size: Pixels,
    ) -> Option<Pixels> {
        let size_bits = f32::from(font_size).to_bits();
        let mut total = px(0.);
        let mut pos = 0usize;
        for r in runs {
            let end = (pos + r.len).min(text.len());
            if pos >= end {
                pos = end;
                continue;
            }
            let font_id = cx.text_system().resolve_font(&r.font);
            for ch in text[pos..end].chars() {
                let cached = self
                    .cache
                    .entry((font_id, size_bits))
                    .or_default()
                    .get(&ch)
                    .copied();
                let w = match cached {
                    Some(w) => w,
                    None => {
                        let w = cx.text_system().advance(font_id, font_size, ch).ok()?.width;
                        self.cache
                            .entry((font_id, size_bits))
                            .or_default()
                            .insert(ch, w);
                        w
                    }
                };
                total += w;
            }
            pos = end;
        }
        Some(total)
    }
}

/// Where this frame will actually be scrolled to: reveal the caret if asked,
/// then clamp to the document.
///
/// This must be settled BEFORE choosing which lines to shape. The clamp can move
/// the viewport *down* (at the document end, to `total_height - viewport_height`)
/// and a caret jump can move it anywhere, so a window chosen from the incoming
/// scroll offset would cover the wrong lines — and unshaped lines paint blank.
pub(crate) fn resolve_scroll(
    layout: &Layout,
    mut scroll_top: Pixels,
    scroll_to_caret: bool,
    cursor: usize,
    viewport_height: Pixels,
) -> Pixels {
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
    scroll_top.clamp(px(0.), max_scroll)
}

/// The fill a find match is painted with, and the outline the *current* one gets
/// on top of it — which is what picks one match out of a page of them.
fn match_colors(current: bool) -> (Hsla, Option<Hsla>) {
    if current {
        (rgba(SEARCH_CURRENT).into(), Some(rgba(SEARCH_RING).into()))
    } else {
        (rgba(SEARCH_MATCH).into(), None)
    }
}

/// A filled band with an outline: the current find match, which needs to stand
/// out from the others without a fill dark enough to swallow the text.
fn band_quad(rect: Bounds<Pixels>, bg: Hsla, ring: Hsla) -> gpui::PaintQuad {
    quad(rect, Corners::all(px(2.)), bg, px(1.), ring, BorderStyle::Solid)
}

/// The text area: the element's rect inset by [`CONTENT_PAD`] on all four sides.
///
/// The element spans the whole window so the overlay scrollbar can hug the right
/// edge and run the full height, so everything text-related — wrap width, the
/// scrollable viewport, painting and hit-testing — works in this rect instead of
/// the element's own bounds.
pub(crate) fn content_rect(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let pad = px(CONTENT_PAD);
    Bounds {
        origin: point(bounds.origin.x + pad, bounds.origin.y + pad),
        size: size(
            (bounds.size.width - pad * 2.).max(px(0.)),
            (bounds.size.height - pad * 2.).max(px(0.)),
        ),
    }
}

/// Map a pointer position in window space into the text layout's coordinates:
/// x measured from the content box's left edge, y from the top of the *document*
/// (so it can be handed straight to `Layout::offset_at`).
///
/// Both are clamped into the content box, which is what makes a click or a drop
/// in the margins read as the nearest edge of the text rather than as a miss —
/// the padding is part of the page.
pub(crate) fn content_point(
    content: Bounds<Pixels>,
    scroll_top: Pixels,
    p: Point<Pixels>,
) -> (Pixels, Pixels) {
    let x = (p.x - content.origin.x).clamp(px(0.), content.size.width);
    let y = (p.y - content.origin.y).clamp(px(0.), content.size.height) + scroll_top;
    (x, y)
}

/// Which source lines must be shaped for this frame: the visible window plus an
/// overscan margin, so ordinary scrolling doesn't need to shape anything new.
pub(crate) fn shape_window(
    lines: &[VisualLine],
    scroll_top: Pixels,
    viewport: Pixels,
) -> Range<usize> {
    if lines.is_empty() {
        return 0..0;
    }
    let overscan = viewport.max(px(200.));
    let at_y = |y: Pixels| {
        lines
            .partition_point(|l| l.top <= y)
            .saturating_sub(1)
            .min(lines.len() - 1)
    };
    let first = at_y((scroll_top - overscan).max(px(0.)));
    let last = at_y(scroll_top + viewport + overscan);
    first..(last + 1).min(lines.len())
}

/// Everything a per-line build needs that doesn't vary line to line.
struct BuildCtx<'a> {
    lines: &'a [&'a str],
    line_off: &'a [usize], // document byte offset of each line's start
    parsed: &'a ParsedDoc,
    cursor: usize,
    cursor_line: usize,
    selection: Option<(usize, usize)>,
    wrap_width: Pixels,
    base_font: gpui::Font,
    mono_font: gpui::Font,
    /// When set, every line is shaped up front (the pre-virtualization
    /// behavior), for A/B-ing rendering via `SLIMNOTES_NO_VIRT=1`.
    shape_all: bool,
}

pub(crate) struct Prepaint {
    layout: Rc<Layout>,
    scroll_top: Pixels,
    cursor: usize,
    selection: Option<(usize, usize)>,
    /// The IME preedit range, underlined while composing.
    marked: Option<(usize, usize)>,
    /// The find's matches and which of them is current, while the search bar is
    /// open and matching something. Read here rather than threaded through the
    /// three prepaint paths: highlighting a match changes nothing about the
    /// layout, so it is only ever a paint input.
    search: Option<(Rc<Vec<Range<usize>>>, usize)>,
    /// Where a click places the caret — the text plus its margins. Gets the
    /// I-beam pointer.
    hitbox: Hitbox,
    /// Links and checkboxes: clicking acts on them, so they get a hand pointer.
    hands: Vec<Hitbox>,
    /// The drag grip showing in the gutter, if any: a grab pointer.
    grip: Option<Hitbox>,
    /// A list item is in flight, so the whole window shows a grabbing pointer.
    grip_held: bool,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl EditorElement {
    /// Finish a prepaint: clamp scroll (keeping the caret in view), persist it,
    /// insert the pointer hitboxes, and package the state for `paint`. Shared by
    /// the cache-hit and rebuild paths so both handle scrolling identically.
    fn finish(
        &mut self,
        layout: Rc<Layout>,
        scroll_top: Pixels,
        bounds: Bounds<Pixels>,
        cursor: usize,
        selection: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let content = content_rect(bounds);
        let viewport_height = content.size.height;
        let scroll_track = bounds.size.height;
        let (marked, grip_line, grip_held, search) = self.editor.update(cx, |e, _| {
            e.scroll_top = scroll_top;
            e.scroll_to_caret = false;
            // Published for hit-testing outside a paint, where the element's
            // bounds aren't in hand — a file dropped on the editor is delivered
            // to the host view, not to this element (see `drop_files`).
            e.content_rect = Some(content);
            // Every prepaint path ends here, so this is the one place the frame's
            // viewport height and scrollbar track are published — they can never
            // go stale, not even on the reuse path (a height-only resize rebuilds
            // nothing).
            e.viewport_height = viewport_height;
            e.scroll_track = scroll_track;
            let marked = e.marked_range.as_ref().map(|r| (r.start, r.end));
            (marked, e.grip_hover, e.grip_drag.is_some(), e.search_paint())
        });
        // Pointer regions. Hitboxes may only be created during prepaint, and
        // `scroll_top` is final by the time we get here, so this is the only
        // correct place for them. Everywhere a click places the caret gets an
        // I-beam; links and checkboxes get a hand over the top of it (`paint`).
        //
        // That is the text *and its margins*, minus the scrollbar strip — and
        // only while a scrollbar is actually shown, which is exactly the
        // condition the mouse-down handler uses to ignore a press there.
        let strip = scrollbar_geom(layout.total_height, viewport_height, scroll_top, scroll_track)
            .map_or(px(0.), |_| px(SCROLLBAR_HIT_W));
        let caret_area = Bounds {
            origin: bounds.origin,
            size: size((bounds.size.width - strip).max(px(0.)), bounds.size.height),
        };
        let hitbox = window.insert_hitbox(caret_area, HitboxBehavior::Normal);
        let visible = scroll_top..scroll_top + viewport_height;
        let hands = layout
            .clickable_rects(visible, cursor)
            .into_iter()
            .filter_map(|r| {
                // To screen space, then clipped to the text area so a region
                // half-scrolled off the edge doesn't reach into the margin.
                let screen = Bounds {
                    origin: point(
                        content.origin.x + r.origin.x,
                        content.origin.y - scroll_top + r.origin.y,
                    ),
                    size: r.size,
                };
                let clipped = screen.intersect(&content);
                (clipped.size.width > px(0.) && clipped.size.height > px(0.))
                    .then(|| window.insert_hitbox(clipped, HitboxBehavior::Normal))
            })
            .collect();
        // The grip's own hitbox, for the grab pointer. Outside the text area by
        // design (it lives in the left padding), so it is inserted separately
        // rather than through `clickable_rects`, which clips to the text.
        let grip = grip_line.and_then(|li| {
            let l = layout.lines.get(li)?;
            let r = l.grip_rect()?;
            let screen = Bounds {
                origin: point(
                    content.origin.x + r.origin.x - px(GRIP_HIT_PAD),
                    content.origin.y - scroll_top + l.top,
                ),
                size: size(r.size.width + px(2. * GRIP_HIT_PAD), l.line_height),
            };
            Some(window.insert_hitbox(screen, HitboxBehavior::Normal))
        });
        Prepaint {
            layout,
            scroll_top,
            cursor,
            selection,
            marked,
            search,
            hitbox,
            hands,
            grip,
            grip_held,
        }
    }

    /// Lay out the given table blocks: build every cell (shaped + source map),
    /// measure column widths across the block, then assign column geometry.
    /// Returns the render for each of their source lines.
    fn build_tables(
        &mut self,
        ctx: &BuildCtx,
        blocks: impl Iterator<Item = (usize, usize)>,
        shaper: &mut Shaper,
        window: &mut Window,
    ) -> HashMap<usize, TableRender> {
        let mut out: HashMap<usize, TableRender> = HashMap::new();
        for (start, end) in blocks {
            let aligns = table_aligns(ctx.lines[start + 1]);
            let row_lis: Vec<usize> = (start..=end).filter(|&li| li != start + 1).collect();
            let mut rows: Vec<Vec<TableCell>> = Vec::new();
            // Shaping inputs kept per cell so a cell that has to be narrowed can
            // be re-shaped below without rebuilding its runs.
            let mut inputs: Vec<Vec<(SharedString, Vec<TextRun>)>> = Vec::new();
            let mut colw: Vec<Pixels> = Vec::new();
            for (ri, &li) in row_lis.iter().enumerate() {
                let weight = if ri == 0 { FontWeight::BOLD } else { FontWeight::NORMAL };
                let mut cells = Vec::new();
                let mut row_inputs = Vec::new();
                for (c, span) in table_cell_spans(ctx.lines[li]).into_iter().enumerate() {
                    let cds = ctx.line_off[li] + span.start;
                    let cde = ctx.line_off[li] + span.end;
                    let cell_text = &ctx.lines[li][span.clone()];
                    // Reuse the memoized document parse for this cell's range
                    // rather than re-parsing the cell.
                    let (spans, markers) = cell_spans(
                        &ctx.parsed.line_spans[li],
                        &ctx.parsed.line_markers[li],
                        cds..cde,
                    );
                    let (display, runs, segs) = build_cell(
                        cell_text,
                        cds,
                        &spans,
                        &markers,
                        &ctx.base_font,
                        weight,
                        ctx.cursor,
                        ctx.selection,
                    );
                    // Pass 1 shapes against the full content width. That both
                    // measures the cell's natural (unwrapped) width — which is
                    // what the columns are sized from — and is already the final
                    // shape for every cell whose column ends up wide enough.
                    let display = SharedString::from(display);
                    let sl = shaper.wrapped(
                        window,
                        &display,
                        px(TABLE_FONT),
                        &runs,
                        ctx.wrap_width,
                    );
                    if c >= colw.len() {
                        colw.resize(c + 1, px(0.));
                    }
                    colw[c] = colw[c].max(sl.unwrapped_layout.width);
                    cells.push(TableCell {
                        shaped: sl,
                        segs,
                        src_start: cds,
                        src_end: cde,
                        x: px(0.), // filled in once column widths are known
                        width: px(0.),
                        align: aligns.get(c).copied().unwrap_or(TableAlign::Left),
                    });
                    row_inputs.push((display, runs));
                }
                rows.push(cells);
                inputs.push(row_inputs);
            }
            for w in &mut colw {
                *w += px(TABLE_PAD_X * 2.);
            }
            // Squeeze the columns into the window if the natural widths overflow
            // it; the cells that no longer fit their column are re-shaped so
            // their text wraps inside the cell instead of running off-screen.
            let colw = fit_columns(&colw, ctx.wrap_width);
            let mut colx: Vec<Pixels> = vec![px(0.)];
            for w in &colw {
                colx.push(*colx.last().unwrap() + *w);
            }
            let mut ri = 0;
            for li in start..=end {
                if li == start + 1 {
                    out.insert(li, TableRender::Sep);
                    continue;
                }
                let mut cells = std::mem::take(&mut rows[ri]);
                for (c, cell) in cells.iter_mut().enumerate() {
                    cell.x = colx[c];
                    cell.width = colw[c];
                    let text_w = (cell.width - px(TABLE_PAD_X * 2.)).max(px(1.));
                    if cell.shaped.unwrapped_layout.width > text_w {
                        let (display, runs) = &inputs[ri][c];
                        cell.shaped =
                            shaper.wrapped(window, display, px(TABLE_FONT), runs, text_w);
                    }
                }
                out.insert(li, TableRender::Row(cells));
                ri += 1;
            }
        }
        out
    }

    /// Build one source line's [`VisualLine`]. `top` is left at zero — the caller
    /// assigns cumulative offsets once all lines are known (see `relayout_tops`).
    fn build_visual_line(
        &mut self,
        ctx: &BuildCtx,
        li: usize,
        table: Option<TableRender>,
        shaper: &mut Shaper,
        bounder: &mut WidthBounder,
        window: &mut Window,
        cx: &mut App,
    ) -> VisualLine {
        let line = ctx.lines[li];
        let offset = ctx.line_off[li];
        let code = &ctx.parsed.code;
        let is_cursor_line = li == ctx.cursor_line;

        // A rendered table row/separator: painted as a grid, no text shaping.
        if let Some(tr) = table {
            let lh = px(TABLE_FONT * 1.5);
            let (segs, height) = match &tr {
                TableRender::Sep => (Vec::new(), px(0.)),
                // As tall as the tallest cell: a cell too wide for its column
                // wraps, and every cell in the row grows to match so the grid
                // stays a grid.
                TableRender::Row(cells) => {
                    let rows = cells.iter().map(|c| c.rows()).max().unwrap_or(1);
                    (
                        vec![Seg { src: 0..line.len(), disp: 0..0, sub: true }],
                        lh * rows as f32 + px(TABLE_PAD_Y * 2.),
                    )
                }
            };
            // A table row paints its cells, never this line's own text, and its
            // height is fixed by the font — so it never needs shaping.
            return VisualLine {
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
                wrapped: None,
                shape_in: None,
                line_height: lh,
                top: px(0.),
                height,
                hang: px(0.),
            };
        }

        let in_block = code[li].in_block || ctx.parsed.fm_end.is_some_and(|e| li <= e);

        // A non-active image line renders the picture in place of its source.
        let image = if !is_cursor_line && !in_block {
            parse_image_line(line).and_then(|embed| {
                let data = self.editor.update(cx, |e, _| e.image_for(&embed.target))?;
                Some((data, embed.size))
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
        let font = if in_block { &ctx.mono_font } else { &ctx.base_font };
        // Hide a ``` fence line unless the caret is inside that same block.
        let hide_fence = fence_hidden(code, ctx.cursor_line, li);
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
        let cursor_col = is_cursor_line.then(|| ctx.cursor - offset);
        let ops = if image.is_some() || hide_fence || hr {
            vec![Op { src: 0..line.len(), repl: Repl::Hide }]
        } else if in_block {
            Vec::new()
        } else {
            let mut v = line_ops(
                line,
                offset,
                &ctx.parsed.line_markers[li],
                &ctx.parsed.line_spans[li],
                ctx.cursor,
                ctx.selection,
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
        // like the gray markup in Obsidian; concealed ones aren't in the display.
        let dim = if in_block {
            Vec::new()
        } else {
            let mut d = dim_ranges(
                line,
                offset,
                &ctx.parsed.line_markers[li],
                &ctx.parsed.line_spans[li],
                ctx.cursor,
                ctx.selection,
            );
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
            build_code_line(line, font, ctx.parsed.code_hl.get(&li).map_or(&[][..], |v| v))
        } else {
            build_line(
                line,
                offset,
                &ops,
                &ctx.parsed.line_spans[li],
                font,
                &dim,
                base_weight,
                checked,
            )
        };
        let display = SharedString::from(display);

        // Virtualization: if this line provably fits one row, its height is known
        // without shaping, so defer the shaping until it scrolls into view. Lines
        // that might wrap are shaped now — their row count *is* their height, and
        // guessing it would drift the scrollbar.
        // An image line's height comes from the picture, not the text, so it is
        // excluded here and falls through to the normal path (images are rare,
        // and this keeps their height and `image` payload in one place).
        let one_row = !ctx.shape_all
            && image.is_none()
            && bounder
                .bound(cx, &display, &runs, font_size)
                .is_some_and(|b| b <= ctx.wrap_width);
        if one_row {
            return VisualLine {
                start: offset,
                source: line.to_string(),
                segs,
                checks,
                code_bg: in_block,
                hr,
                left_inset,
                quote_bars,
                table: None,
                image: None,
                wrapped: None,
                shape_in: Some(ShapeIn {
                    display,
                    runs,
                    font_size,
                    width: ctx.wrap_width,
                }),
                line_height: lh,
                top: px(0.),
                height: lh, // exactly one row
                // A single-row line has no continuation rows to hang-indent.
                hang: px(0.),
            };
        }

        let mut wrapped = shaper.wrapped(window, &display, font_size, &runs, ctx.wrap_width);

        // Hanging indent for wrapped list items: align continuation rows under
        // the text (after the bullet/checkbox), not the marker. Source offset
        // just past the `- `/`- [ ] ` prefix, mapped to display x.
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
        let mut shape_width = ctx.wrap_width;
        if let Some(se) = hang_src_end {
            let disp = src_to_disp(&segs, se.min(line.len()));
            hang = wrapped.position_for_index(disp, lh).map(|p| p.x).unwrap_or_default();
            // Re-shape narrower so shifted continuation rows don't overflow.
            if hang > px(0.) && wrapped.size(lh).height > lh {
                shape_width = ctx.wrap_width - hang;
                wrapped = shaper.wrapped(window, &display, font_size, &runs, shape_width);
            }
        }

        // How big to draw an image. An embed's own `|size` is taken as written
        // — that is the point of asking — and otherwise the picture is shown at
        // its natural size. Either way it is then fitted to the page width.
        //
        // There is deliberately no height cap. One used to be applied here, and
        // because a screenshot is taller than it is wide relative to the page,
        // it was what decided the size of virtually every image, shrinking each
        // one to a fraction of the available width. A tall image is simply tall;
        // the page scrolls.
        let image = image.map(|(data, want)| {
            let iw = (data.size(0).width.0 as f32).max(1.0);
            let ih = (data.size(0).height.0 as f32).max(1.0);
            let (w, h) = match want {
                // `|width` alone keeps the aspect ratio; `|width x height` is
                // taken literally, stretch and all.
                Some(s) => (s.width, s.height.unwrap_or(ih * s.width / iw)),
                None => (iw, ih),
            };
            let scale = (f32::from(ctx.wrap_width) / w).min(1.0);
            (data, size(px(w * scale), px(h * scale)))
        });
        let height = match &image {
            Some((_, sz)) => sz.height + px(IMAGE_PAD * 2.),
            None => wrapped.size(lh).height,
        };
        // Hanging indent only means anything with continuation rows to shift, and
        // paint/`x_offset` both ignore it on a single-row line. Normalize it to
        // zero there so this path agrees with the deferred (one-row) path above
        // field-for-field — otherwise the two differ for every single-row list
        // item, which makes `SLIMNOTES_NO_VIRT=1` a useless control for proving
        // that virtualization changes nothing but *which* lines get shaped.
        if height <= lh {
            hang = px(0.);
        }
        VisualLine {
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
            wrapped: Some(wrapped),
            shape_in: Some(ShapeIn {
                display,
                runs,
                font_size,
                width: shape_width,
            }),
            line_height: lh,
            top: px(0.),
            height,
            hang,
        }
    }
}

/// Shape any not-yet-shaped line in `range`, so everything about to be painted
/// has a `WrappedLine`. This never changes a line's height: a deferred line was
/// proven to fit one row, and shaping it yields exactly that one row — which is
/// what keeps `total_height` and the scroll geometry stable as you scroll.
/// Whether a line drawn at `screen_top` with `height` shows any of itself
/// between `top`/`bottom`.
///
/// Those edges must be the **element's**, not the inset text box's: nothing
/// clips a line to the text box, so a line overlapping the top/bottom padding is
/// visible in it. Testing against the text box instead made a line paint nothing
/// for the ~32px it spent crossing the padding and then appear once it reached
/// the text area, i.e. "text missing until you scroll a bit".
pub(crate) fn line_on_screen(
    screen_top: Pixels,
    height: Pixels,
    top: Pixels,
    bottom: Pixels,
) -> bool {
    screen_top + height >= top && screen_top <= bottom
}

/// Whether any line in `range` still needs shaping before it could be painted —
/// i.e. exactly what [`fill_shapes`] would fill in.
///
/// Asked of the layout itself rather than tracked as a "shaped from..to" range.
/// A summary range cannot express what is actually shaped: the caret's line is
/// always shaped even when it sits far outside the viewport, so any hull
/// spanning both claims the lines in between are ready when they are not — and
/// an unshaped line paints blank. (That was a real bug: jump down the scrollbar
/// with the caret left at the top, then scroll back slightly, and a screenful of
/// text vanished until you scrolled far enough to force a refill.)
pub(crate) fn missing_shapes(lines: &[VisualLine], range: Range<usize>) -> bool {
    let n = lines.len();
    lines[range.start.min(n)..range.end.min(n)]
        .iter()
        .any(|l| l.wrapped.is_none() && l.shape_in.is_some())
}

fn fill_shapes(
    lines: &mut [VisualLine],
    range: Range<usize>,
    shaper: &mut Shaper,
    window: &mut Window,
) -> usize {
    let mut filled = 0;
    let n = lines.len();
    for l in &mut lines[range.start.min(n)..range.end.min(n)] {
        if l.wrapped.is_some() {
            continue;
        }
        let Some(si) = l.shape_in.clone() else { continue };
        l.wrapped = Some(shaper.wrapped(window, &si.display, si.font_size, &si.runs, si.width));
        filled += 1;
    }
    filled
}

/// Assign cumulative `top` offsets from `from` onward and return the new total
/// height. Reveal can change a line's wrap-row count, so a patched line shifts
/// everything below it.
fn relayout_tops(lines: &mut [VisualLine], from: usize) -> Pixels {
    let mut top = if from == 0 {
        px(0.)
    } else {
        lines[from - 1].top + lines[from - 1].height
    };
    for l in &mut lines[from..] {
        l.top = top;
        top += l.height;
    }
    top
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
                e.buffer.version(),
            )
        };
        // Text lives in the inset rect; the element's own bounds are the window
        // (so the scrollbar can reach its edges).
        let content = content_rect(bounds);
        let wrap_width = content.size.width.max(px(80.));
        let viewport_height = content.size.height;
        let sig = LayoutSig { version, width: f32::from(wrap_width).to_bits() };
        let reveal = Reveal { cursor, sel: selection };

        // Fast path: text, width, caret and selection all unchanged (a pure
        // scroll) — reuse the layout. Scrolling may have exposed lines whose
        // shaping was deferred, so top up the shape set before reusing; heights
        // don't change, so the geometry (and thus the scrollbar) is untouched.
        let cached = self.editor.update(cx, |e, _| {
            (e.layout_sig == Some(sig) && e.layout_reveal == Some(reveal))
                .then(|| e.layout.clone())
                .flatten()
        });
        if let Some(mut layout) = cached {
            // Settle the scroll first: the window to shape depends on where this
            // frame actually lands, not on the incoming offset.
            let scroll_top =
                resolve_scroll(&layout, scroll_top, scroll_to_caret, cursor, viewport_height);
            let caret_line = layout.line_index_at_offset(cursor).unwrap_or(0);
            let want = shape_window(&layout.lines, scroll_top, viewport_height);
            let need = missing_shapes(&layout.lines, want.clone())
                || missing_shapes(&layout.lines, caret_line..caret_line + 1);
            if need {
                let mut shaper =
                    Shaper::patching(self.editor.update(cx, |e, _| std::mem::take(&mut e.shapes)));
                let l = Rc::make_mut(&mut layout);
                fill_shapes(&mut l.lines, want.clone(), &mut shaper, window);
                fill_shapes(&mut l.lines, caret_line..caret_line + 1, &mut shaper, window);
                let shapes = shaper.finish();
                self.editor.update(cx, |e, _| {
                    e.layout = Some(layout.clone());
                    e.shapes = shapes;
                });
            }
            return self.finish(
                layout,
                scroll_top,
                bounds,
                cursor,
                selection,
                window,
                cx,
            );
        }

        let text = self.editor.read(cx).text.clone();
        let parsed = self.editor.update(cx, |e, _| e.ensure_parsed());
        let lines_vec: Vec<&str> = text.split('\n').collect();
        let mut line_off = Vec::with_capacity(lines_vec.len());
        {
            let mut acc = 0usize;
            for l in &lines_vec {
                line_off.push(acc);
                acc += l.len() + 1;
            }
        }
        let mut base_font = window.text_style().font();
        base_font.family = UI_FONT.into();
        let mut mono_font = base_font.clone();
        mono_font.family = MONO_FONT.into();
        let ctx = BuildCtx {
            lines: &lines_vec,
            line_off: &line_off,
            parsed: &parsed,
            cursor,
            cursor_line: offset_to_line_col(&text, cursor).0,
            selection,
            wrap_width,
            base_font,
            mono_font,
            shape_all: std::env::var_os("SLIMNOTES_NO_VIRT").is_some(),
        };
        let mut bounder = WidthBounder::new();
        let tables = table_blocks(&lines_vec);

        // Patch path: only the caret/selection moved, so only the lines whose
        // reveal can differ are rebuilt — the rest of the layout is reused. This
        // makes typing and caret movement cost independent of document length.
        let patch = self.editor.update(cx, |e, _| {
            if e.layout_sig != Some(sig) {
                return None;
            }
            let reveal = e.layout_reveal?;
            // Take rather than clone the Rc: with the editor's reference gone
            // this is the only one, so `Rc::make_mut` below patches in place
            // instead of copying every line. Bailing out is safe — the full
            // rebuild below stores a fresh layout.
            Some((e.layout.take()?, reveal))
        });
        if let Some((old_layout, old_reveal)) = patch {
            let dirty = dirty_reveal_lines(
                &lines_vec,
                &line_off,
                &parsed.code,
                &tables,
                old_reveal,
                reveal,
            );
            // A layout built for a different text length can't be patched.
            if let Some(dirty) = dirty.filter(|_| old_layout.lines.len() == lines_vec.len()) {
                let mut shaper =
                    Shaper::patching(self.editor.update(cx, |e, _| std::mem::take(&mut e.shapes)));
                let mut layout = old_layout;
                let lines = &mut Rc::make_mut(&mut layout).lines;
                // Table geometry spans a whole block, so rebuild any block a
                // dirty line falls in.
                let mut table_map = self.build_tables(
                    &ctx,
                    tables
                        .iter()
                        .copied()
                        .filter(|&(s, e)| dirty.iter().any(|&li| (s..=e).contains(&li))),
                    &mut shaper,
                    window,
                );
                for &li in &dirty {
                    let tr = table_map.remove(&li);
                    lines[li] =
                        self.build_visual_line(&ctx, li, tr, &mut shaper, &mut bounder, window, cx);
                }
                let from = dirty.first().copied().unwrap_or(0);
                let total = relayout_tops(lines, from);
                // Rebuilt lines may have deferred their shaping; make sure the
                // visible ones (and the caret's) are shaped before painting.
                let caret_line = ctx.cursor_line.min(lines.len().saturating_sub(1));
                let scroll_top = {
                    let l = Rc::make_mut(&mut layout);
                    l.total_height = total;
                    resolve_scroll(l, scroll_top, scroll_to_caret, cursor, viewport_height)
                };
                let lines = &mut Rc::make_mut(&mut layout).lines;
                let want = shape_window(lines, scroll_top, viewport_height);
                fill_shapes(lines, want.clone(), &mut shaper, window);
                fill_shapes(lines, caret_line..caret_line + 1, &mut shaper, window);
                let shapes = shaper.finish();
                self.editor.update(cx, |e, _| {
                    e.layout = Some(layout.clone());
                    e.layout_reveal = Some(reveal);
                    e.shapes = shapes;
                });
                return self.finish(
                layout,
                scroll_top,
                bounds,
                cursor,
                selection,
                window,
                cx,
            );
            }
        }

        // Slow path: text or width changed — rebuild every line. Shaped lines are
        // still served from the persistent cache, so a rebuild only shapes the
        // lines whose text/style actually changed; the rest are reused even across
        // the fast-path frames that would otherwise evict GPUI's own cache.
        let started = std::time::Instant::now();
        let mut shaper =
            Shaper::rebuilding(self.editor.update(cx, |e, _| std::mem::take(&mut e.shapes)));
        let mut table_map = self.build_tables(&ctx, tables.iter().copied(), &mut shaper, window);
        // Pass 1: build every line and settle its exact height, shaping only the
        // lines that might wrap (a line proven to fit one row defers shaping).
        let mut lines = Vec::with_capacity(lines_vec.len());
        for li in 0..lines_vec.len() {
            let tr = table_map.remove(&li);
            lines.push(self.build_visual_line(&ctx, li, tr, &mut shaper, &mut bounder, window, cx));
        }
        let total_height = relayout_tops(&mut lines, 0);
        let shaped_in_pass1 = lines.iter().filter(|l| l.wrapped.is_some()).count();

        // Pass 2: with the geometry known, shape what is actually on screen (plus
        // the caret's line). Heights are already final, so this cannot move
        // anything — it only fills in glyphs for painting.
        let mut layout = Layout {
            lines,
            total_height,
            links: parsed.links.clone(),
        };
        let caret_line = ctx.cursor_line.min(layout.lines.len().saturating_sub(1));
        let scroll_top =
            resolve_scroll(&layout, scroll_top, scroll_to_caret, cursor, viewport_height);
        let want = shape_window(&layout.lines, scroll_top, viewport_height);
        fill_shapes(&mut layout.lines, want.clone(), &mut shaper, window);
        fill_shapes(&mut layout.lines, caret_line..caret_line + 1, &mut shaper, window);

        // Dump the geometry so a virtualized build can be diffed against
        // `SLIMNOTES_NO_VIRT=1`: identical output proves the deferred heights are
        // exact, which is what keeps the scrollbar and scroll-max stable.
        if let Some(path) = std::env::var_os("SLIMNOTES_DUMP_GEOM") {
            use std::io::Write;
            if let Ok(mut f) = std::fs::File::create(path) {
                let _ = writeln!(
                    f,
                    "total_height {:.3} scroll_top {:.3} viewport {:.3}",
                    f32::from(layout.total_height),
                    f32::from(scroll_top),
                    f32::from(viewport_height)
                );
                for (i, l) in layout.lines.iter().enumerate() {
                    let _ = writeln!(
                        f,
                        "{i} top={:.3} height={:.3} lh={:.3} hang={:.3} shaped={} code_bg={} table={}",
                        f32::from(l.top),
                        f32::from(l.height),
                        f32::from(l.line_height),
                        f32::from(l.hang),
                        u8::from(l.wrapped.is_some()),
                        u8::from(l.code_bg),
                        u8::from(l.table.is_some()),
                    );
                }
            }
        }

        if std::env::var_os("SLIMNOTES_PERF").is_some() {
            let shaped = layout.lines.iter().filter(|l| l.wrapped.is_some()).count();
            eprintln!(
                "slimnotes: built {} lines, shaped {} ({} to measure height, {} for the viewport), \
                 {:?}{}",
                layout.lines.len(),
                shaped,
                shaped_in_pass1,
                shaped - shaped_in_pass1,
                started.elapsed(),
                if ctx.shape_all { " [NO_VIRT]" } else { "" },
            );
        }

        let layout = Rc::new(layout);
        let shapes = shaper.finish();
        self.editor.update(cx, |e, _| {
            e.layout = Some(layout.clone());
            e.layout_sig = Some(sig);
            e.layout_reveal = Some(reveal);
            e.shapes = shapes;
        });
        self.finish(
                layout,
                scroll_top,
                bounds,
                cursor,
                selection,
                window,
                cx,
            )
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
        // `bounds` is the whole window (so the scrollbar can reach its edges);
        // the text lives in the inset rect, so everything below works in
        // `content` and only the scrollbar uses `bounds`.
        let content = content_rect(bounds);

        // Route platform text input (and IME composition) at the text area. Must
        // happen during paint — `handle_input` asserts that phase — and it is what
        // delivers typed characters to the buffer.
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(content, self.editor.clone()),
            cx,
        );

        // Pointer shapes. An I-beam over the text, as in any text editor, scoped
        // to the text area — the margins and the scrollbar strip keep the arrow.
        // Then a hand over links and checkboxes: requests are resolved
        // last-one-wins, so these override the I-beam where they overlap.
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.hitbox);
        for hand in &prepaint.hands {
            window.set_cursor_style(CursorStyle::PointingHand, hand);
        }
        // The list-item grip: grab where it is showing. While an item is in
        // flight the grabbing pointer covers everything, so it doesn't flick
        // back to an I-beam the moment the pointer leaves the handle it holds.
        if prepaint.grip_held {
            window.set_cursor_style(CursorStyle::ClosedHand, &prepaint.hitbox);
        } else if let Some(grip) = &prepaint.grip {
            window.set_cursor_style(CursorStyle::OpenHand, grip);
        }

        let st = prepaint.scroll_top;
        // Cull against the *window*, not the inset text box. Nothing clips a line
        // to the content rect, so a line overlapping the top/bottom padding is
        // genuinely visible there and has to be painted: culling on the content
        // box made a line paint nothing for the ~32px it spent crossing the
        // padding, then pop into existence on reaching the text area — text
        // "missing until you scroll a bit". Straddling lines always drew into the
        // padding, so this makes the two consistent.
        let top_edge = bounds.origin.y;
        let bottom_edge = bounds.origin.y + bounds.size.height;
        // Debug hatch: report any line that is on screen but was never shaped.
        // Such a line paints blank, so this turns "text is missing" — which is
        // easy to miss and hard to attribute — into a loud, greppable signal.
        let check_shapes = std::env::var_os("SLIMNOTES_CHECK_SHAPES").is_some();

        for line in &prepaint.layout.lines {
            let screen_top = content.origin.y - st + line.top;
            if !line_on_screen(screen_top, line.height, top_edge, bottom_edge) {
                continue;
            }

            // Code-block panel: a contiguous full-width shaded band.
            if line.code_bg {
                let rect = Bounds {
                    origin: point(content.origin.x - px(8.), screen_top),
                    size: size(content.size.width + px(16.), line.height),
                };
                window.paint_quad(fill(rect, rgb(CODE_PANEL)));
            }

            // Blockquote bars, one per nesting level, in the left inset.
            for k in 0..line.quote_bars {
                let bx = content.origin.x + px(QUOTE_INDENT * k as f32) + px(3.);
                let rect = Bounds {
                    origin: point(bx, screen_top),
                    size: size(px(3.), line.height),
                };
                window.paint_quad(fill(rect, rgb(MUTED)));
            }

            // A band behind a document byte range, one quad per wrapped row it
            // covers — how both the selection and the find's matches are drawn.
            // Painted *after* the line's run backgrounds (see below) so it shows
            // over inline-code/highlight backgrounds, which are opaque; hence a
            // closure invoked at the right point in each text-paint path rather
            // than here.
            let paint_band = |win: &mut Window,
                              (ss, se): (usize, usize),
                              bg: Hsla,
                              ring: Option<Hsla>| {
                let Some(wrapped) = line.wrapped.as_ref() else { return };
                let (ls, le) = (line.start, line.start + line.source.len());
                if se > ls && ss < le {
                    let a = src_to_disp(&line.segs, ss.max(ls) - ls);
                    let b = src_to_disp(&line.segs, se.min(le) - ls);
                    let lh = line.line_height;
                    for (x0, x1, row_y) in row_bands(wrapped, a, b, lh) {
                        let shift = line.x_offset(row_y);
                        let rect = Bounds {
                            origin: point(content.origin.x + x0 + shift, screen_top + row_y),
                            size: size((x1 - x0).max(px(3.)), lh),
                        };
                        win.paint_quad(match ring {
                            Some(ring) => band_quad(rect, bg, ring),
                            None => fill(rect, bg),
                        });
                    }
                }
            };

            // The find's matches, then the selection over them: a selection is
            // the thing the user is holding, so it wins where they overlap.
            let paint_highlights = |win: &mut Window| {
                if let Some((matches, current)) = &prepaint.search {
                    for (ix, m) in search_in(matches, line.start, line.start + line.source.len()) {
                        let (bg, ring) = match_colors(ix == *current);
                        paint_band(win, (m.start, m.end), bg, ring);
                    }
                }
                if let Some(sel) = prepaint.selection {
                    paint_band(win, sel, rgb(SELECTION).into(), None);
                }
            };

            // IME preedit: underline the uncommitted composition so the in-progress
            // text is distinguishable from committed text.
            let paint_marked = |win: &mut Window| {
                let Some((ms, me)) = prepaint.marked else { return };
                let Some(wrapped) = line.wrapped.as_ref() else { return };
                let (ls, le) = (line.start, line.start + line.source.len());
                if me <= ls || ms >= le {
                    return;
                }
                let lh = line.line_height;
                let a = src_to_disp(&line.segs, ms.max(ls) - ls);
                let b = src_to_disp(&line.segs, me.min(le) - ls);
                let pa = wrapped.position_for_index(a, lh).unwrap_or_default();
                let pb = wrapped.position_for_index(b, lh).unwrap_or_default();
                // Only the common case (a preedit within one visual row) is drawn;
                // compositions are short, so a wrapped preedit isn't worth the rows.
                if pa.y != pb.y {
                    return;
                }
                let shift = line.x_offset(pa.y);
                let rect = Bounds {
                    origin: point(
                        content.origin.x + pa.x + shift,
                        screen_top + pa.y + lh - px(2.),
                    ),
                    size: size((pb.x - pa.x).max(px(2.)), px(1.)),
                };
                win.paint_quad(fill(rect, rgb(TEXT)));
            };

            if let Some((data, sz)) = &line.image {
                let rect = Bounds {
                    origin: point(content.origin.x, screen_top + px(IMAGE_PAD)),
                    size: *sz,
                };
                let _ = window.paint_image(rect, Corners::all(px(4.)), data.clone(), 0, false);
                continue;
            }

            // Horizontal rule: a thin centered line across the content width.
            if line.hr {
                paint_highlights(window);
                paint_marked(window);
                let rect = Bounds {
                    origin: point(content.origin.x, screen_top + line.height / 2. - px(1.)),
                    size: size(content.size.width, px(2.)),
                };
                window.paint_quad(fill(rect, rgb(MUTED)));
                continue;
            }

            // Table row: each cell as a bordered, padded, aligned box.
            if let Some(TableRender::Row(cells)) = &line.table {
                for cell in cells {
                    let cx0 = content.origin.x + cell.x;
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
                    let lh = line.line_height;
                    let text_origin = point(cx0 + cell.text_x(), screen_top + px(TABLE_PAD_Y));
                    // A band over a cell-local display range, one quad per
                    // wrapped row it covers — the cell's counterpart to the
                    // line-level `paint_band` above.
                    let cell_band = |win: &mut Window, a: usize, b: usize, bg, ring| {
                        for (x0, x1, y) in row_bands(&cell.shaped, a, b, lh) {
                            let rect = Bounds {
                                origin: point(text_origin.x + x0, screen_top + px(1.) + y),
                                size: size((x1 - x0).max(px(3.)), lh),
                            };
                            win.paint_quad(match ring {
                                Some(ring) => band_quad(rect, bg, ring),
                                None => fill(rect, bg),
                            });
                        }
                    };
                    // paint_background draws inline code/highlight run backgrounds
                    // (WrappedLine::paint only draws glyphs).
                    let _ = cell.shaped.paint_background(
                        text_origin,
                        lh,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    // A table row has no `wrapped` line of its own — every cell
                    // is separately shaped — so the `paint_highlights` above
                    // cannot draw the find's matches or the selection over it;
                    // each cell highlights itself here. After the run
                    // backgrounds (which are opaque) so highlighted inline code
                    // still reads as highlighted, and before the glyphs.
                    if let Some((matches, current)) = &prepaint.search {
                        for (ix, m) in search_in(matches, cell.src_start, cell.src_end) {
                            let covered =
                                cell_selection(cell.src_start, cell.src_end, Some((m.start, m.end)));
                            let (bg, ring) = match_colors(ix == *current);
                            match covered {
                                Some(CellSel::Whole) => {
                                    let rect = Bounds {
                                        origin: point(cx0 + px(1.), screen_top + px(1.)),
                                        size: size(
                                            (cell.width - px(2.)).max(px(0.)),
                                            (line.height - px(2.)).max(px(0.)),
                                        ),
                                    };
                                    window.paint_quad(match ring {
                                        Some(ring) => band_quad(rect, bg, ring),
                                        None => fill(rect, bg),
                                    });
                                }
                                Some(CellSel::Part(r)) => {
                                    let a = src_to_disp(&cell.segs, r.start);
                                    let b = src_to_disp(&cell.segs, r.end);
                                    cell_band(window, a, b, bg, ring);
                                }
                                None => continue,
                            }
                        }
                    }
                    match cell_selection(cell.src_start, cell.src_end, prepaint.selection) {
                        Some(CellSel::Whole) => {
                            // Inset by the border so the grid stays visible
                            // through the band, which is what makes a
                            // multi-row selection still look like a table.
                            let rect = Bounds {
                                origin: point(cx0 + px(1.), screen_top + px(1.)),
                                size: size(
                                    (cell.width - px(2.)).max(px(0.)),
                                    (line.height - px(2.)).max(px(0.)),
                                ),
                            };
                            window.paint_quad(fill(rect, rgb(SELECTION)));
                        }
                        Some(CellSel::Part(r)) => {
                            let a = src_to_disp(&cell.segs, r.start);
                            let b = src_to_disp(&cell.segs, r.end);
                            cell_band(window, a, b, rgb(SELECTION).into(), None);
                        }
                        None => {}
                    }
                    let _ = cell
                        .shaped
                        .paint(text_origin, lh, TextAlign::Left, None, window, cx);
                }
                continue;
            }
            if line.table.is_some() {
                continue; // separator row: nothing to paint
            }

            let lh = line.line_height;
            // Second invariant: the height a line reserved in the layout must be
            // the height its shaped text actually occupies. Too small and it
            // overlaps whatever follows; too large and it leaves a gap. Either
            // way the deferred-height bound was wrong for this line.
            if check_shapes && line.image.is_none() {
                if let Some(w) = line.wrapped.as_ref() {
                    let shaped_h = w.size(lh).height;
                    if (f32::from(shaped_h) - f32::from(line.height)).abs() > 0.5 {
                        eprintln!(
                            "slimnotes: HEIGHT MISMATCH start={} reserved={:.1} shaped={:.1} lh={:.1}",
                            line.start,
                            f32::from(line.height),
                            f32::from(shaped_h),
                            f32::from(lh),
                        );
                    }
                }
            }
            let base_x = content.origin.x + line.left_inset;
            // An unshaped line has nothing to draw. It is off-screen by
            // construction (prepaint shapes the visible window), so this is just
            // belt-and-braces against painting a line we deliberately skipped.
            let Some(wrapped) = line.wrapped.as_ref() else {
                // Report *any* on-screen line that would paint nothing, not just
                // ones that still hold shaping inputs — a line missing both is
                // exactly as blank, and gating on `shape_in` would hide it.
                if check_shapes && !line.source.is_empty() {
                    eprintln!(
                        "slimnotes: UNSHAPED VISIBLE LINE start={} top={:.1} h={:.1} \
                         (scroll_top={:.1}, shape_in={})",
                        line.start,
                        f32::from(line.top),
                        f32::from(line.height),
                        f32::from(st),
                        u8::from(line.shape_in.is_some()),
                    );
                }
                continue;
            };
            if line.hang > px(0.) && line.height > lh {
                // Hanging indent: paint row 0 at the margin, continuation rows
                // shifted right, using vertical content masks to split them.
                let fx = content.origin.x - px(16.);
                let fw = content.size.width + px(32.);
                let mask0 = ContentMask {
                    bounds: Bounds { origin: point(fx, screen_top), size: size(fw, lh) },
                };
                window.with_content_mask(Some(mask0), |w| {
                    let o = point(base_x, screen_top);
                    let _ = wrapped.paint_background(o, lh, TextAlign::Left, None, w, cx);
                    paint_highlights(w);
                    paint_marked(w);
                    let _ = wrapped.paint(o, lh, TextAlign::Left, None, w, cx);
                });
                let mask1 = ContentMask {
                    bounds: Bounds {
                        origin: point(fx, screen_top + lh),
                        size: size(fw, line.height - lh),
                    },
                };
                window.with_content_mask(Some(mask1), |w| {
                    let o = point(base_x + line.hang, screen_top);
                    let _ = wrapped.paint_background(o, lh, TextAlign::Left, None, w, cx);
                    paint_highlights(w);
                    paint_marked(w);
                    let _ = wrapped.paint(o, lh, TextAlign::Left, None, w, cx);
                });
            } else {
                let o = point(base_x, screen_top);
                let _ = wrapped.paint_background(o, lh, TextAlign::Left, None, window, cx);
                paint_highlights(window);
                paint_marked(window);
                let _ = wrapped.paint(o, lh, TextAlign::Left, None, window, cx);
            }

            // Checkboxes: painted over their reserved slot (always on row 0).
            for chk in &line.checks {
                let p = wrapped
                    .position_for_index(chk.disp.start, lh)
                    .unwrap_or_default();
                let side = lh * 0.62;
                let pad = (lh - side) / 2.;
                let rect = Bounds {
                    origin: point(content.origin.x + p.x + px(1.), screen_top + pad),
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
            let sy = content.origin.y - st + cy;
            let caret = Bounds {
                origin: point(content.origin.x + x, sy + px(1.)),
                size: size(px(2.), lh - px(2.)),
            };
            window.paint_quad(fill(caret, rgb(CARET)));
        }

        // The list-item drag grip: the handle in the gutter, plus — while an
        // item is in flight — a band over the item and a line where it will
        // land. After the text loop, so all three sit above the text.
        {
            let e = self.editor.read(cx);
            if let Some(d) = &e.grip_drag {
                if let Some((top, bottom)) = d
                    .run
                    .items
                    .get(d.from)
                    .and_then(|item| item_span(&prepaint.layout, item))
                {
                    // Wider than the text, like the code panel, so the whole
                    // item reads as one thing that has been picked up.
                    let rect = Bounds {
                        origin: point(content.origin.x - px(8.), content.origin.y - st + top),
                        size: size(content.size.width + px(16.), bottom - top),
                    };
                    window.paint_quad(fill(rect, rgba(GRIP_BAND)));
                }
                if let Some(y) = e.grip_drop_y(d) {
                    let rect = Bounds {
                        origin: point(content.origin.x, content.origin.y - st + y - px(1.)),
                        size: size(content.size.width, px(2.)),
                    };
                    window.paint_quad(fill(rect, rgb(ACCENT)));
                }
            }
            // Six dots, two by three. Drawn as quads rather than a glyph so the
            // handle doesn't depend on a font having one.
            if let Some(l) = e.grip_hover.and_then(|li| prepaint.layout.lines.get(li)) {
                if let Some(r) = l.grip_rect() {
                    let color = if e.grip_drag.is_some() { ACCENT } else { MUTED };
                    let x0 = content.origin.x + r.origin.x;
                    let y0 = content.origin.y - st + l.top + r.origin.y;
                    for row in 0..3 {
                        for col in 0..2 {
                            let dot = Bounds {
                                origin: point(
                                    x0 + px(GRIP_PITCH * col as f32),
                                    y0 + px(GRIP_PITCH * row as f32),
                                ),
                                size: size(px(GRIP_DOT), px(GRIP_DOT)),
                            };
                            window.paint_quad(quad(
                                dot,
                                Corners::all(px(GRIP_DOT / 2.)),
                                rgb(color),
                                px(0.),
                                transparent_black(),
                                BorderStyle::Solid,
                            ));
                        }
                    }
                }
            }
        }

        // Overlay scrollbar, painted last so it sits above the text. It hugs the
        // window's right edge and its track spans the full window height, so it
        // is drawn against `bounds` while its proportion comes from the inset
        // text viewport. Sized from the layout's exact `total_height`, which
        // virtualization keeps correct without shaping the whole document.
        if let Some(g) = scrollbar_geom(
            prepaint.layout.total_height,
            content.size.height,
            st,
            bounds.size.height,
        ) {
            let hot = self.editor.read(cx).scrollbar_hover;
            let rect = Bounds {
                origin: point(
                    bounds.origin.x + bounds.size.width - px(SCROLLBAR_W + SCROLLBAR_MARGIN),
                    bounds.origin.y + g.thumb_top,
                ),
                size: size(px(SCROLLBAR_W), g.thumb_len),
            };
            window.paint_quad(quad(
                rect,
                Corners::all(px(SCROLLBAR_W / 2.)),
                rgba(if hot { SCROLLBAR_HOVER } else { SCROLLBAR }),
                px(0.),
                transparent_black(),
                BorderStyle::Solid,
            ));
        }

        // Mouse: click-to-place, drag-to-select.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseDownEvent, phase, window, cx| {
                // The whole element, not just the inset text: the padding reads as
                // part of the page, so clicking it places the caret at the nearest
                // text position. Only the scrollbar strip is excluded (below).
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Left
                    || !bounds.contains(&ev.position)
                {
                    return;
                }
                editor.update(cx, |e, cx| {
                    // A click anywhere while the menu is open just dismisses it —
                    // the items are real elements and handle their own clicks
                    // (they sit above this element, so a hit on one never reaches
                    // here). Don't also move the caret.
                    if e.table_menu.is_some() {
                        e.table_menu = None;
                        cx.notify();
                        return;
                    }
                    // A press in the scrollbar strip belongs to the scrollbar
                    // (handled below); don't also move the caret or start a
                    // selection. Checked here rather than relying on handler
                    // ordering, which GPUI doesn't guarantee.
                    if e.scrollbar().is_some() && in_scrollbar_strip(&bounds, ev.position) {
                        return;
                    }
                    // Likewise a press on a list item's drag grip: it picks the
                    // item up (below) and must not also move the caret.
                    if e.grip_hit(ev.position, content).is_some() {
                        return;
                    }
                    let Some(layout) = e.layout.clone() else { return };
                    // A click in the margins is treated as a click on the nearest
                    // edge of the text (`content_point` clamps), landing it at the
                    // start/end of the nearest visible line.
                    let (x, content_y) = content_point(content, e.scroll_top, ev.position);
                    // ...but a margin click must not *activate* anything. A
                    // checkbox at the start of its line sits within a few px of
                    // the content edge, so a clamped click would land inside its
                    // (deliberately generous) target and toggle it; likewise for a
                    // link at a line's start or end.
                    let on_text = content.contains(&ev.position);
                    e.focus_handle.focus(window);
                    e.goal_x = None; // a click recomputes the goal column next time
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
                        e.dragging = false;
                        e.scroll_to_caret = false;
                        cx.notify();
                        return;
                    }
                    // A click on a rendered checkbox toggles it instead of moving the caret.
                    if on_text && !ev.modifiers.shift {
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
                    if on_text && !ev.modifiers.shift {
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
                    e.dragging = true;
                    cx.notify();
                });
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if ev.pressed_button != Some(MouseButton::Left) {
                    // The button came up somewhere we never saw it (released
                    // outside the window, say). End the drag here rather than
                    // leave it live — otherwise autoscroll would keep running.
                    editor.update(cx, |e, cx| {
                        if e.dragging {
                            e.dragging = false;
                            e.set_autoscroll(None, cx);
                        }
                    });
                    return;
                }
                editor.update(cx, |e, cx| {
                    if !e.dragging {
                        return;
                    }
                    let Some(layout) = e.layout.clone() else { return };
                    let local = point(
                        ev.position.x - content.origin.x,
                        ev.position.y - content.origin.y,
                    );
                    // Dragging past the top or bottom of the text keeps scrolling
                    // for as long as the pointer stays there, so a selection can
                    // run past the visible page. The trigger zone starts at the
                    // *content* edge — inside the window by CONTENT_PAD — so it
                    // engages before the pointer leaves the window at all, and
                    // doesn't depend on motion events still arriving from outside.
                    if drag_scroll_step(local.y, e.viewport_height) != px(0.) {
                        e.set_autoscroll(Some(local), cx);
                    } else {
                        e.set_autoscroll(None, cx);
                    }
                    e.cursor = layout.offset_at(local.x, local.y + e.scroll_top);
                    e.goal_x = None;
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
                editor.update(cx, |e, cx| {
                    e.dragging = false;
                    e.set_autoscroll(None, cx); // stop scrolling on release
                    if e.scrollbar_drag.take().is_some() {
                        cx.notify(); // the thumb goes back to its idle shade
                    }
                });
            }
        });
        // The list-item drag grip. Hovering the gutter beside a bullet or task
        // item offers a handle; dragging it moves the item, with everything
        // nested under it, up or down its list. The text handlers above stay
        // out of the way by testing `grip_hit` themselves rather than relying
        // on handler ordering, which GPUI doesn't guarantee.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                editor.update(cx, |e, cx| {
                    if e.grip_drag.is_some() {
                        return; // in flight: the drag handler owns the pointer
                    }
                    // Not while a button is down (that is a text selection), and
                    // not under an open menu.
                    let over = (ev.pressed_button.is_none()
                        && e.table_menu.is_none()
                        && bounds.contains(&ev.position))
                    .then(|| e.gutter_item_at(ev.position, content))
                    .flatten();
                    if over != e.grip_hover {
                        e.grip_hover = over;
                        cx.notify();
                    }
                });
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                    return;
                }
                editor.update(cx, |e, cx| {
                    let Some(li) = e.grip_hit(ev.position, content) else { return };
                    e.focus_handle.focus(window);
                    e.start_grip_drag(li, cx);
                });
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                editor.update(cx, |e, cx| {
                    if e.grip_drag.is_none() {
                        return;
                    }
                    if ev.pressed_button != Some(MouseButton::Left) {
                        e.finish_grip_drag(cx); // released outside the window
                        return;
                    }
                    let local = point(
                        ev.position.x - content.origin.x,
                        ev.position.y - content.origin.y,
                    );
                    // The same edge ramp a text drag uses, so an item can be
                    // carried past the visible page of a long list.
                    if drag_scroll_step(local.y, e.viewport_height) != px(0.) {
                        e.set_autoscroll(Some(local), cx);
                    } else {
                        e.set_autoscroll(None, cx);
                    }
                    e.update_grip_target(local.y + e.scroll_top, cx);
                });
            }
        });
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                    return;
                }
                editor.update(cx, |e, cx| e.finish_grip_drag(cx));
            }
        });
        // Scrollbar: grab the thumb to drag, or click the track to page.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseDownEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Left
                    || !in_scrollbar_strip(&bounds, ev.position)
                {
                    return;
                }
                editor.update(cx, |e, cx| {
                    // An open menu swallows the click (the text handler closes it).
                    if e.table_menu.is_some() {
                        return;
                    }
                    let Some(g) = e.scrollbar() else { return };
                    let y = ev.position.y - bounds.origin.y; // relative to the track
                    if y >= g.thumb_top && y <= g.thumb_top + g.thumb_len {
                        // Remember where in the thumb we grabbed, so it follows
                        // the pointer 1:1 instead of jumping.
                        e.scrollbar_drag = Some(y - g.thumb_top);
                    } else {
                        // Track click: jump straight to that spot in the
                        // document (centring the thumb on the click) rather than
                        // paging toward it — in a long note the thumb is at its
                        // minimum length, so paging would take dozens of clicks
                        // to cross the track. Then hand over to the drag path
                        // with the grab at the thumb's middle, so press-and-move
                        // keeps scrubbing from where you clicked.
                        let grab = g.thumb_len / 2.;
                        e.scroll_top = scroll_for_thumb_top(
                            e.total_height(),
                            e.viewport_height,
                            y - grab,
                            g.track,
                        );
                        e.scrollbar_drag = Some(grab);
                    }
                    e.scrollbar_hover = true;
                    e.dragging = false; // never a text selection
                    e.scroll_to_caret = false; // and don't snap back to the caret
                    cx.notify();
                });
            }
        });
        // Scrollbar drag + hover highlight.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                editor.update(cx, |e, cx| {
                    if let Some(grab) = e.scrollbar_drag {
                        if ev.pressed_button == Some(MouseButton::Left) {
                            let y = ev.position.y - bounds.origin.y - grab;
                            e.scroll_top = scroll_for_thumb_top(
                                e.total_height(),
                                e.viewport_height,
                                y,
                                e.scroll_track,
                            );
                            e.scroll_to_caret = false;
                            cx.notify();
                            return;
                        }
                        e.scrollbar_drag = None; // released outside the window
                    }
                    let over =
                        e.scrollbar().is_some() && in_scrollbar_strip(&bounds, ev.position);
                    if over != e.scrollbar_hover {
                        e.scrollbar_hover = over;
                        cx.notify();
                    }
                });
            }
        });
        // Right-click on a table cell opens its add/delete row/column menu.
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |ev: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Right
                    || !content.contains(&ev.position)
                {
                    return;
                }
                editor.update(cx, |e, cx| {
                    let Some(layout) = e.layout.clone() else { return };
                    let x = ev.position.x - content.origin.x;
                    let content_y = ev.position.y - content.origin.y + e.scroll_top;
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
        // Scroll wheel. Deliberately the full window rect, not the text area: the
        // wheel should work over the margins and over the scrollbar too.
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
                    let vh = e.viewport_height;
                    let max = e
                        .layout
                        .as_ref()
                        .map(|l| max_scroll(l.total_height, vh))
                        .unwrap_or(px(0.));
                    e.scroll_top = (e.scroll_top - dy).clamp(px(0.), max);
                    e.scroll_to_caret = false;
                    cx.notify();
                });
            }
        });
    }
}
