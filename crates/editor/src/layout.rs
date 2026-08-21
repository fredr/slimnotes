//! The visual (wrapped) lines shared between paint and the entity, plus the
//! text/offset helpers used to navigate the document.

use std::ops::Range;
use std::sync::Arc;

use gpui::{point, px, size, Bounds, Pixels, RenderImage, Size, WrappedLine};

use crate::*;

/// `Clone` so a built layout can be patched line-by-line when only the caret /
/// selection moved (see `dirty_reveal_lines`); the heavy fields (`WrappedLine`,
/// `RenderImage`, shaped cells) are all `Arc`-backed, so a clone is cheap.
#[derive(Clone)]
pub(crate) struct VisualLine {
    pub(crate) start: usize,          // document byte offset of the line start
    pub(crate) source: String,        // raw source of the line
    pub(crate) segs: Vec<Seg>,        // source<->display transform (conceal + substitution)
    pub(crate) checks: Vec<CheckHit>, // clickable checkbox glyphs on this line
    pub(crate) code_bg: bool,         // paint a full-width code panel behind this line
    pub(crate) hr: bool,              // render a horizontal rule instead of text
    pub(crate) left_inset: Pixels,    // whole-line indent (blockquotes)
    pub(crate) quote_bars: u8,        // number of blockquote bars to paint at the left
    pub(crate) table: Option<TableRender>, // rendered as a table row instead of text
    pub(crate) image: Option<(Arc<RenderImage>, Size<Pixels>)>, // rendered in place of text
    /// The shaped line — `None` for a line that is off-screen and provably one
    /// row tall, which is the whole point of virtualization: its `height` is
    /// already exact, so shaping (the expensive part) is skipped until it scrolls
    /// into view. Anything painted or hit-tested has been shaped.
    pub(crate) wrapped: Option<WrappedLine>,
    /// The display string + shaping inputs, kept so an unshaped line can be
    /// shaped later (when it scrolls in) without rebuilding it from source.
    pub(crate) shape_in: Option<ShapeIn>,
    pub(crate) line_height: Pixels,
    pub(crate) top: Pixels,    // y offset from content top
    pub(crate) height: Pixels, // total height incl. wrapped rows
    pub(crate) hang: Pixels,   // hanging indent: wrapped rows shift right by this much
}

/// What a deferred line needs in order to be shaped later: exactly the inputs
/// `shape_text` takes. Cheap to keep (the runs are already built).
#[derive(Clone)]
pub(crate) struct ShapeIn {
    pub(crate) display: gpui::SharedString,
    pub(crate) runs: Vec<gpui::TextRun>,
    pub(crate) font_size: Pixels,
    pub(crate) width: Pixels,
}

impl VisualLine {
    /// The clickable rect of one checkbox, in line-local coordinates (x from the
    /// content left edge, y from this line's top). Deliberately a little larger
    /// than the painted box — it is a small target — and shared by the click
    /// hit-test and the pointer shape, so what looks clickable is clickable.
    ///
    /// `None` for an unshaped line, which has no glyph positions.
    pub(crate) fn check_rect(&self, chk: &CheckHit) -> Option<Bounds<Pixels>> {
        let wrapped = self.wrapped.as_ref()?;
        let p0 = wrapped.position_for_index(chk.disp.start, self.line_height)?;
        let p1 = wrapped
            .position_for_index(chk.disp.end, self.line_height)
            .unwrap_or(p0);
        Some(Bounds {
            origin: point(p0.x - px(3.), p0.y),
            size: size(p1.x + px(4.) - (p0.x - px(3.)), self.line_height),
        })
    }

    /// Total x-offset for a point at local y: the whole-line inset plus, for
    /// wrapped continuation rows of a list item, the hanging indent.
    pub(crate) fn x_offset(&self, local_y: Pixels) -> Pixels {
        let hang = if self.hang > px(0.) && local_y >= self.line_height {
            self.hang
        } else {
            px(0.)
        };
        self.left_inset + hang
    }
}

#[derive(Clone)]
pub(crate) struct Layout {
    /// One entry per source line, in order — so `lines[li]` is source line `li`
    /// (table rows and separators included), which is what lets the patch path
    /// index straight into it.
    pub(crate) lines: Vec<VisualLine>,
    pub(crate) total_height: Pixels,
    pub(crate) links: Links, // document source range -> link target
}

impl Layout {
    /// Content-space rects of everything a left click *acts on* rather than
    /// placing the caret: rendered links and checkbox slots. The pointer turns
    /// into a hand over these (see `element::paint`), so the shape always
    /// promises exactly what the click handlers deliver.
    ///
    /// Restricted to `visible` (a content-space y range) since off-screen lines
    /// are unshaped and have no geometry, and there is nothing to hover anyway.
    pub(crate) fn clickable_rects(
        &self,
        visible: Range<Pixels>,
        cursor: usize,
    ) -> Vec<Bounds<Pixels>> {
        let mut out = Vec::new();
        // Binary-search the visible slice rather than scanning the document —
        // this runs every prepaint, and a linear pass would give back what
        // virtualization bought.
        let (Some(first), Some(last)) = (
            self.line_index_at_y(visible.start),
            self.line_index_at_y(visible.end),
        ) else {
            return out;
        };
        let on_screen =
            |l: &VisualLine| l.top + l.height >= visible.start && l.top <= visible.end;

        for l in &self.lines[first..=last] {
            for c in &l.checks {
                if let Some(r) = l.check_rect(c) {
                    out.push(Bounds {
                        origin: point(r.origin.x, l.top + r.origin.y),
                        size: r.size,
                    });
                }
            }
        }

        // Links are only clickable while the caret is elsewhere: on the caret's
        // own line the raw source shows and a click edits it instead of opening
        // the link, so that line must not offer a hand (see the mouse handler).
        let caret_line = self.line_index_at_offset(cursor);
        // Source range spanned by the visible lines, so a link elsewhere in the
        // document is rejected by two comparisons instead of a binary search.
        let seen = self.lines[first].start
            ..self.lines[last].start + self.lines[last].source.len();
        for (src, _) in self.links.iter() {
            if src.start < seen.start || src.start >= seen.end {
                continue;
            }
            let Some(li) = self.line_index_at_offset(src.start) else { continue };
            if Some(li) == caret_line {
                continue;
            }
            let l = &self.lines[li];
            if !on_screen(l) {
                continue;
            }
            let Some(wrapped) = l.wrapped.as_ref() else { continue };
            let line_end = l.start + l.source.len();
            if src.start < l.start || src.start >= line_end {
                continue;
            }
            let lh = l.line_height;
            let a = src_to_disp(&l.segs, src.start - l.start);
            let b = src_to_disp(&l.segs, src.end.min(line_end) - l.start);
            let Some(pa) = wrapped.position_for_index(a, lh) else { continue };
            let pb = wrapped.position_for_index(b, lh).unwrap_or(pa);
            let full_w = wrapped.size(lh).width;
            let mut row = |x0: Pixels, x1: Pixels, y: Pixels| {
                if x1 > x0 {
                    out.push(Bounds {
                        origin: point(x0 + l.x_offset(y), l.top + y),
                        size: size(x1 - x0, lh),
                    });
                }
            };
            // A link normally sits on one row; a long one can wrap, so cover
            // each row it spans.
            if pa.y == pb.y {
                row(pa.x, pb.x, pa.y);
            } else {
                row(pa.x, full_w, pa.y);
                let mut y = pa.y + lh;
                while y + px(0.5) < pb.y {
                    row(px(0.), full_w, y);
                    y += lh;
                }
                row(px(0.), pb.x, pb.y);
            }
        }
        out
    }

    /// The link target whose source range contains `offset`, if any.
    pub(crate) fn link_at(&self, offset: usize) -> Option<LinkTarget> {
        self.links
            .iter()
            .find(|(r, _)| r.start <= offset && offset < r.end)
            .map(|(_, t)| t.clone())
    }
}

impl Layout {
    /// Index of the line whose vertical span contains `content_y` (the last line
    /// if `y` is past the end). Binary search over the cumulative `top`s, so this
    /// stays O(log n) on a long document.
    pub(crate) fn line_index_at_y(&self, content_y: Pixels) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        // First line whose top is strictly greater than y, minus one.
        let i = self.lines.partition_point(|l| l.top <= content_y);
        Some(i.saturating_sub(1).min(self.lines.len() - 1))
    }

    pub(crate) fn line_at_y(&self, content_y: Pixels) -> Option<&VisualLine> {
        self.line_index_at_y(content_y).map(|i| &self.lines[i])
    }

    /// Index of the line containing document byte `offset`. Binary search over
    /// the line `start`s (which are ascending).
    pub(crate) fn line_index_at_offset(&self, offset: usize) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        let i = self.lines.partition_point(|l| l.start <= offset);
        Some(i.saturating_sub(1).min(self.lines.len() - 1))
    }

    /// Document offset at a content-space point.
    pub(crate) fn offset_at(&self, x: Pixels, content_y: Pixels) -> usize {
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
        // An unshaped line can't be hit-tested; it is off-screen by construction,
        // so the best answer is its start (never panic on a missing shape).
        let Some(wrapped) = l.wrapped.as_ref() else {
            return l.start;
        };
        let local_y = content_y - l.top;
        // Undo the hanging indent before hit-testing continuation rows.
        let local_x = (x - l.x_offset(local_y)).max(px(0.));
        let local = point(local_x, local_y);
        let disp = wrapped
            .closest_index_for_position(local, l.line_height)
            .unwrap_or_else(|e| e);
        l.start + disp_to_src(&l.segs, disp)
    }

    /// Content-space caret position (x, y-top, line_height) for a document offset.
    pub(crate) fn caret(&self, cursor: usize) -> Option<(Pixels, Pixels, Pixels)> {
        let l = self.lines.get(self.line_index_at_offset(cursor)?)?;
        // Table row: place the caret inside the cell whose source contains it
        // (snapping into the nearest cell if the offset landed in a pipe gap).
        if let Some(TableRender::Row(cells)) = &l.table {
            if let Some(cell) = cells
                .iter()
                .find(|c| cursor >= c.src_start && cursor <= c.src_end)
                .or_else(|| in_table_line(l, cursor).then(|| pick_cell(cells, px(0.))).flatten())
            {
                let off = cursor.clamp(cell.src_start, cell.src_end);
                let disp = src_to_disp(&cell.segs, off - cell.src_start);
                let cx = cell.x + cell.text_x() + cell.shaped.x_for_index(disp);
                return Some((cx, l.top + px(TABLE_PAD_Y), px(TABLE_FONT * 1.5)));
            }
            return Some((px(0.), l.top + px(TABLE_PAD_Y), px(TABLE_FONT * 1.5)));
        }
        // A table separator (`| --- |`) renders nothing and is zero-height, so it
        // has no caret position. Reporting one would put a zero-height caret on an
        // invisible row and let vertical motion "land" there; callers treat `None`
        // as "don't move / don't scroll", which is the behavior we want.
        if l.table.is_some() {
            return None;
        }
        let disp = src_to_disp(&l.segs, cursor.saturating_sub(l.start));
        // The caret's line is always shaped (it's in the shape set), so this
        // fallback only covers a caret on a line that hasn't been shaped yet —
        // report the line's left edge rather than panicking.
        let p = l
            .wrapped
            .as_ref()
            .and_then(|w| w.position_for_index(disp, l.line_height))
            .unwrap_or_default();
        Some((p.x + l.x_offset(p.y), l.top + p.y, l.line_height))
    }
}

impl Layout {
    /// The (document line-start offset, column index) of the table cell at a
    /// content-space point, if the point is on a rendered table row.
    pub(crate) fn table_cell_at(&self, x: Pixels, content_y: Pixels) -> Option<(usize, usize)> {
        let l = self.line_at_y(content_y)?;
        let TableRender::Row(cells) = l.table.as_ref()? else {
            return None;
        };
        let col = cells
            .iter()
            .position(|c| x >= c.x && x < c.x + c.width)
            .unwrap_or(if cells.is_empty() || x < cells[0].x {
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
pub(crate) fn pick_cell(cells: &[TableCell], x: Pixels) -> Option<&TableCell> {
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .find(|c| x >= c.x && x < c.x + c.width)
        .or_else(|| if x < cells[0].x { cells.first() } else { cells.last() })
}

pub(crate) fn line_height(size: Pixels) -> Pixels {
    size * 1.5_f32
}

// ---------------------------------------------------------------------------
// Reveal state + which lines a caret/selection move can dirty.
// ---------------------------------------------------------------------------

/// The caret + selection a layout was built with. Marker reveal depends on both
/// (a marker shows when the caret sits in its span, or the selection overlaps
/// it), so a layout is only reusable as-is while these are unchanged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reveal {
    pub(crate) cursor: usize,
    pub(crate) sel: Option<(usize, usize)>,
}

/// Past this many dirty lines, patching loses to just rebuilding everything.
pub(crate) const MAX_DIRTY_LINES: usize = 200;

/// The source line containing byte `off`.
fn line_at(line_off: &[usize], off: usize) -> usize {
    line_off.partition_point(|&s| s <= off).saturating_sub(1)
}

/// The lines whose rendered output can differ between reveal states `old` and
/// `new` — everything else in the layout is reusable untouched. Returns `None`
/// when the set is too large to be worth patching (caller does a full rebuild).
///
/// Conservative by construction: a touched line is expanded to its enclosing
/// structural block, because reveal is not strictly per-line — an inline span
/// may cross a soft line break (`*foo\nbar*`), a table's column widths depend on
/// every row, and ``` fence visibility depends on which block holds the caret.
pub(crate) fn dirty_reveal_lines(
    lines: &[&str],
    line_off: &[usize],
    code: &[CodeLine],
    tables: &[(usize, usize)],
    old: Reveal,
    new: Reveal,
) -> Option<Vec<usize>> {
    let n = lines.len();
    if n == 0 {
        return Some(Vec::new());
    }
    let mut dirty = vec![false; n];
    fn mark(dirty: &mut [bool], li: usize) {
        if li < dirty.len() {
            dirty[li] = true;
        }
    }

    // Every line the caret or the selection touches, in either state.
    for st in [old, new] {
        mark(&mut dirty, line_at(line_off, st.cursor));
        if let Some((s, e)) = st.sel {
            for li in line_at(line_off, s)..=line_at(line_off, e) {
                mark(&mut dirty, li);
            }
        }
    }

    // A ``` fence is concealed unless the caret is inside its own block, so when
    // the caret changes block, both blocks' lines change.
    let block_of = |st: Reveal| {
        let li = line_at(line_off, st.cursor).min(n - 1);
        if code.get(li).is_some_and(|c| c.in_block) {
            code[li].block
        } else {
            0
        }
    };
    let (ob, nb) = (block_of(old), block_of(new));
    if ob != nb {
        for (li, c) in code.iter().enumerate().take(n) {
            if c.block != 0 && (c.block == ob || c.block == nb) {
                mark(&mut dirty, li);
            }
        }
    }

    // Expand each touched line to its enclosing block: a whole table, a whole
    // fenced code block, or the surrounding paragraph.
    for li in (0..n).filter(|&li| dirty[li]).collect::<Vec<_>>() {
        if let Some(&(s, e)) = tables.iter().find(|&&(s, e)| (s..=e).contains(&li)) {
            for l in s..=e {
                mark(&mut dirty, l);
            }
            continue;
        }
        if code.get(li).is_some_and(|c| c.in_block) {
            let b = code[li].block;
            for (l, c) in code.iter().enumerate().take(n) {
                if c.block == b {
                    mark(&mut dirty, l);
                }
            }
            continue;
        }
        // Paragraph: the contiguous run of non-blank lines around `li`.
        let mut s = li;
        while s > 0 && !lines[s - 1].trim().is_empty() {
            s -= 1;
        }
        let mut e = li;
        while e + 1 < n && !lines[e + 1].trim().is_empty() {
            e += 1;
        }
        for l in s..=e {
            mark(&mut dirty, l);
        }
    }

    let out: Vec<usize> = (0..n).filter(|&li| dirty[li]).collect();
    (out.len() <= MAX_DIRTY_LINES).then_some(out)
}

// ---------------------------------------------------------------------------
// Text/offset helpers (byte offsets, char-boundary aware).
// ---------------------------------------------------------------------------

pub(crate) fn prev_boundary(text: &str, offset: usize) -> usize {
    text[..offset]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// The byte range of the "word" at `offset`, for double-click selection: the
/// run of alphanumeric/`_` characters the click touches (on either side), or —
/// if it touches none — the single character under the cursor.
pub(crate) fn word_at(text: &str, offset: usize) -> Range<usize> {
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

/// Whether `c` is part of a "word" for word-wise motion (mirrors [`word_at`]'s
/// classification: alphanumerics and underscore).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset one word to the right of `offset`: skip any whitespace, then skip
/// a run of word chars (or a single non-word char). Crosses line breaks, which
/// count as whitespace. Used by Ctrl/Alt+Right.
pub(crate) fn word_right(text: &str, offset: usize) -> usize {
    let mut i = offset.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    while let Some(c) = text[i..].chars().next() {
        if c.is_whitespace() {
            i += c.len_utf8();
        } else {
            break;
        }
    }
    match text[i..].chars().next() {
        Some(c) if is_word_char(c) => {
            while let Some(c) = text[i..].chars().next() {
                if is_word_char(c) {
                    i += c.len_utf8();
                } else {
                    break;
                }
            }
        }
        Some(c) => i += c.len_utf8(),
        None => {}
    }
    i
}

/// Byte offset one word to the left of `offset`: skip whitespace backward, then
/// skip a run of word chars (or a single non-word char). Used by Ctrl/Alt+Left.
pub(crate) fn word_left(text: &str, offset: usize) -> usize {
    let mut i = offset.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    while let Some(c) = text[..i].chars().next_back() {
        if c.is_whitespace() {
            i -= c.len_utf8();
        } else {
            break;
        }
    }
    match text[..i].chars().next_back() {
        Some(c) if is_word_char(c) => {
            while let Some(c) = text[..i].chars().next_back() {
                if is_word_char(c) {
                    i -= c.len_utf8();
                } else {
                    break;
                }
            }
        }
        Some(c) => i -= c.len_utf8(),
        None => {}
    }
    i
}

pub(crate) fn offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
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

pub(crate) fn line_start(text: &str, line: usize) -> usize {
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

pub(crate) fn line_str(text: &str, line: usize) -> &str {
    text.split('\n').nth(line).unwrap_or("")
}

pub(crate) fn line_col_to_offset(text: &str, line: usize, col_byte: usize) -> usize {
    let start = line_start(text, line);
    let l = line_str(text, line);
    let mut col = col_byte.min(l.len());
    while col > 0 && !l.is_char_boundary(col) {
        col -= 1;
    }
    start + col
}
