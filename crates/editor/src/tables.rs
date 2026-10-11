//! Everything tables: parsing, rendering geometry, structural edits and the
//! cell context menu.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    point, px, Font, FontWeight, Pixels, Point, RenderImage, SharedString, Size, TextRun, WrappedLine,
};

use crate::*;

pub(crate) const TABLE_PAD_X: f32 = 10.0; // horizontal cell padding (px)
pub(crate) const TABLE_PAD_Y: f32 = 5.0; // vertical cell padding (px)
pub(crate) const TABLE_FONT: f32 = 16.0; // table cell font size (px)
pub(crate) const TABLE_BORDER: u32 = 0x3a3a3a; // cell border color
/// Narrowest a column may be squeezed to when a table is too wide for the
/// window (content + both pads). Below this, wrapping turns cells into a column
/// of single letters, which is worse than overflowing.
pub(crate) const TABLE_MIN_COL: f32 = 60.0;

/// An open table context menu: the window-space point to anchor it at and which
/// cell (source line + column) it acts on.
#[derive(Clone, Copy)]
pub(crate) struct TableMenu {
    pub(crate) pos: Point<Pixels>,
    pub(crate) line: usize,
    pub(crate) col: usize,
}

/// The context-menu items, in display order. The hit-test in the mouse handler
/// and the visual rows in `render` both index this, so they can't drift.
pub(crate) const TABLE_MENU_ITEMS: [(&str, TableOp); 6] = [
    ("Insert row above", TableOp::RowAbove),
    ("Insert row below", TableOp::RowBelow),
    ("Insert column left", TableOp::ColLeft),
    ("Insert column right", TableOp::ColRight),
    ("Delete row", TableOp::DeleteRow),
    ("Delete column", TableOp::DeleteColumn),
];
pub(crate) const TABLE_MENU_W: f32 = 190.0;
pub(crate) const TABLE_MENU_ITEM_H: f32 = 30.0;
pub(crate) const TABLE_MENU_VPAD: f32 = 6.0;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum TableAlign {
    Left,
    Center,
    Right,
}

/// One display line of a table cell. A GFM row can't hold a real newline, so a
/// cell breaks its text at each `<br>` instead (see [`cell_breaks`]), and each
/// line between them is shaped — and soft-wrapped — on its own.
#[derive(Clone)]
pub(crate) struct CellLine {
    pub(crate) disp: usize, // cell display offset of the line's first byte
    pub(crate) shaped: WrappedLine,
}

/// A rendered table cell: its shaped text plus column geometry (x/width are
/// relative to the table's left edge) and the source mapping needed to place the
/// caret / hit-test clicks inside it for in-cell editing.
///
/// Positions inside a cell (`caret_pos`, `disp_at`, `bands`, `line_origin`)
/// are relative to the cell box's left edge and the top of its text, i.e. just
/// below the top pad.
#[derive(Clone)]
pub(crate) struct TableCell {
    /// The cell's display lines, never empty. Each is shaped against the cell's
    /// own content width, so a long one wraps onto further rows instead of
    /// running out past the table (and the window).
    pub(crate) lines: Vec<CellLine>,
    pub(crate) segs: Vec<Seg>,   // cell-local source <-> display transform
    pub(crate) src_start: usize, // document offset of the cell's content start
    pub(crate) src_end: usize,   // document offset of the cell's content end
    pub(crate) x: Pixels,
    pub(crate) width: Pixels,
    pub(crate) align: TableAlign,
    /// A cell holding only an image draws the picture (at this size) in place
    /// of its text, which is then empty. See [`cell_image`].
    pub(crate) image: Option<(Arc<RenderImage>, Size<Pixels>)>,
}

/// How many visual rows a shaped line occupies.
fn line_rows(w: &WrappedLine) -> usize {
    w.wrap_boundaries().len() + 1
}

impl TableCell {
    /// Horizontal offset within the cell box of content `w` wide: the
    /// alignment pad.
    ///
    /// A wrapped line fills its box by definition, so all three alignments
    /// collapse to the left pad there — alignment only shows on lines short
    /// enough to have slack.
    fn align_x(&self, w: Pixels) -> Pixels {
        match self.align {
            TableAlign::Left => px(TABLE_PAD_X),
            TableAlign::Right => (self.width - px(TABLE_PAD_X) - w).max(px(TABLE_PAD_X)),
            TableAlign::Center => ((self.width - w) / 2.).max(px(TABLE_PAD_X)),
        }
    }

    /// Horizontal offset of the cell's image within the cell box.
    pub(crate) fn image_x(&self) -> Pixels {
        self.align_x(self.image.as_ref().map_or(px(0.), |(_, sz)| sz.width))
    }

    /// Where display line `i` starts. Each line is aligned on its own, as the
    /// lines of an HTML table cell are.
    pub(crate) fn line_origin(&self, i: usize, line_height: Pixels) -> Point<Pixels> {
        let above: usize = self.lines[..i].iter().map(|l| line_rows(&l.shaped)).sum();
        point(self.align_x(self.lines[i].shaped.width()), line_height * above as f32)
    }

    /// How many visual rows the cell's text occupies.
    pub(crate) fn rows(&self) -> usize {
        self.lines.iter().map(|l| line_rows(&l.shaped)).sum()
    }

    /// Height of the cell's content, padding excluded.
    pub(crate) fn content_height(&self, line_height: Pixels) -> Pixels {
        match &self.image {
            Some((_, sz)) => sz.height,
            None => line_height * self.rows() as f32,
        }
    }

    /// The widest display line, unwrapped — what the cell asks of its column.
    pub(crate) fn natural_width(&self) -> Pixels {
        self.lines
            .iter()
            .map(|l| l.shaped.unwrapped_layout.width)
            .fold(px(0.), Pixels::max)
    }

    /// Index of the display line holding display offset `disp`.
    fn line_of(&self, disp: usize) -> usize {
        self.lines.iter().rposition(|l| l.disp <= disp).unwrap_or(0)
    }

    /// Caret position for display offset `disp`.
    pub(crate) fn caret_pos(&self, disp: usize, line_height: Pixels) -> Point<Pixels> {
        let i = self.line_of(disp);
        let l = &self.lines[i];
        let p = wrapped_caret_pos(&l.shaped, disp - l.disp, line_height).unwrap_or_default();
        self.line_origin(i, line_height) + p
    }

    /// The display offset nearest `local`. Clamped into the cell's text: a
    /// click in the empty space under a short cell lands at the end of its last
    /// line, one beside a short line at that line's end.
    pub(crate) fn disp_at(&self, local: Point<Pixels>, line_height: Pixels) -> usize {
        let mut top = px(0.);
        for (i, l) in self.lines.iter().enumerate() {
            let h = line_height * line_rows(&l.shaped) as f32;
            if local.y < top + h || i + 1 == self.lines.len() {
                let o = self.line_origin(i, line_height);
                let at = point(
                    (local.x - o.x).max(px(0.)),
                    (local.y - top).clamp(px(0.), h - line_height),
                );
                let d = l
                    .shaped
                    .closest_index_for_position(at, line_height)
                    .unwrap_or_else(|e| e);
                return l.disp + d;
            }
            top += h;
        }
        0
    }

    /// The bands covering display range `a..b`: one `(x0, x1, y)` per visual
    /// row it touches, across the cell's lines — the cell's counterpart to
    /// [`row_bands`].
    pub(crate) fn bands(&self, a: usize, b: usize, line_height: Pixels) -> Vec<(Pixels, Pixels, Pixels)> {
        let mut out = Vec::new();
        for (i, l) in self.lines.iter().enumerate() {
            let (la, lb) = (a.max(l.disp), b.min(l.disp + l.shaped.len()));
            if la > lb {
                continue;
            }
            let o = self.line_origin(i, line_height);
            for (x0, x1, y) in row_bands(&l.shaped, la - l.disp, lb - l.disp, line_height) {
                out.push((o.x + x0, o.x + x1, o.y + y));
            }
        }
        out
    }
}

/// Split a cell's display string, and its runs, into its lines: at the
/// newlines its `<br>`s were displayed as (see [`Repl::Break`]), which are
/// themselves dropped. One `(display offset, text, runs)` per line.
pub(crate) fn split_cell_lines(
    display: &str,
    runs: &[TextRun],
) -> Vec<(usize, SharedString, Vec<TextRun>)> {
    let ends = display.match_indices('\n').map(|(i, _)| i).chain([display.len()]);
    let mut start = 0;
    let mut out = Vec::new();
    for end in ends {
        let mut line_runs = Vec::new();
        let mut pos = 0;
        for run in runs {
            let (a, b) = (pos.max(start), (pos + run.len).min(end));
            if a < b {
                line_runs.push(TextRun { len: b - a, ..run.clone() });
            }
            pos += run.len;
        }
        out.push((start, SharedString::from(display[start..end].to_string()), line_runs));
        start = end + 1;
    }
    out
}

/// Document ranges of the `<br>` tags in a table row line: the line breaks of
/// its cells. `<br/>` and `<br />` count too, in any case. One inside a code
/// span, or escaped (`\<br>`), is literal text.
pub(crate) fn cell_breaks(line: &str, line_start: usize, spans: &[Span]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = line[i..].find('<') {
        let s = i + rel;
        i = s + 1;
        let Some(len) = br_tag_len(&line[s..]) else { continue };
        let doc = line_start + s;
        let in_code = spans
            .iter()
            .any(|sp| sp.attr == Attr::Code && sp.start <= doc && doc < sp.end);
        if in_code || escaped(line, s) {
            continue;
        }
        out.push(doc..doc + len);
        i = s + len;
    }
    out
}

/// Length of the `<br>` / `<br/>` / `<br />` tag at the start of `s`, if any.
fn br_tag_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 4 || b[0] != b'<' || !b[1..3].eq_ignore_ascii_case(b"br") {
        return None;
    }
    let mut j = 3;
    while b.get(j) == Some(&b' ') {
        j += 1;
    }
    if b.get(j) == Some(&b'/') {
        j += 1;
    }
    (b.get(j) == Some(&b'>')).then_some(j + 1)
}

/// The image a table cell holds, if the cell is solely one. Inside a table the
/// `|` before a size has to be escaped (`![[img.png\|300]]`), since a bare one
/// would end the cell, so it is unescaped before the usual image-line parse.
pub(crate) fn cell_image(cell: &str) -> Option<ImageEmbed> {
    parse_image_line(&cell.replace("\\|", "|"))
}

/// What a selection covers of one table cell.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellSel {
    /// The whole cell: fill its box, so a row (or a whole table) inside the
    /// selection reads as one continuous band rather than scattered word
    /// highlights over the cell text.
    Whole,
    /// Part of the cell's text, as a *cell-local* source byte range (i.e.
    /// relative to [`TableCell::src_start`], ready for `src_to_disp`).
    Part(Range<usize>),
}

/// How `sel` (a document byte range) covers the cell spanning `cs..ce`.
///
/// Kept separate from painting so the cases can be tested without a shaped
/// line: a cell is only involved if the selection reaches into it, and an
/// *empty* cell counts as covered when the selection passes through its
/// position — otherwise a selected row would have holes where cells are blank.
pub(crate) fn cell_selection(
    cs: usize,
    ce: usize,
    sel: Option<(usize, usize)>,
) -> Option<CellSel> {
    let (ss, se) = sel?;
    if ss >= se {
        return None; // a collapsed range is a caret, not a selection
    }
    let touches = if cs == ce { ss <= cs && se > cs } else { se > cs && ss < ce };
    if !touches {
        return None;
    }
    if ss <= cs && se >= ce {
        return Some(CellSel::Whole);
    }
    Some(CellSel::Part(ss.max(cs) - cs..se.min(ce) - cs))
}

/// How a source line participates in a rendered table.
#[derive(Clone)]
pub(crate) enum TableRender {
    Sep, // the `| --- |` separator row: draws nothing (0 height)
    Row(Vec<TableCell>),
}

/// A table separator row: `| --- | :--: |` — cells of only `-`/`:`/spaces,
/// each containing a `-`.
pub(crate) fn is_table_sep(line: &str) -> bool {
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
pub(crate) fn table_blocks(lines: &[&str]) -> Vec<(usize, usize)> {
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
pub(crate) fn split_table_row(line: &str) -> Vec<String> {
    table_cell_spans(line).iter().map(|r| line[r.clone()].to_string()).collect()
}

/// Line-local byte ranges of each cell's *trimmed content* (mirrors
/// [`split_table_row`] but keeps source positions, so a rendered cell can map
/// clicks/caret back to the raw markdown for in-cell editing).
///
/// As in GFM, a `|` right after a backslash is cell content, not a column
/// break — that is how a pipe gets into a cell, e.g. `![[img.png\|300]]`.
pub(crate) fn table_cell_spans(line: &str) -> Vec<Range<usize>> {
    let b = line.as_bytes();
    let is_sep = |i: usize| b[i] == b'|' && (i == 0 || b[i - 1] != b'\\');
    let mut start = line.len() - line.trim_start().len();
    let mut end = line.trim_end().len();
    if start < end && is_sep(start) {
        start += 1;
    }
    if end > start && is_sep(end - 1) {
        end -= 1;
    }
    let mut spans = Vec::new();
    let mut pos = start;
    for i in start..=end {
        if i < end && !is_sep(i) {
            continue;
        }
        let part = &line[pos..i];
        let cs = pos + (part.len() - part.trim_start().len());
        let ce = i - (part.len() - part.trim_end().len());
        spans.push(cs..ce.max(cs));
        pos = i + 1; // skip the '|'
    }
    spans
}

/// Column widths (padding included) that make the table fit `avail`.
///
/// While the natural widths fit, they are used as-is — a table only ever
/// narrows, never stretches, so a small table keeps looking like a small table.
/// Past that, every column is shrunk in proportion to its natural width, so the
/// wide columns give up the most, with two floors: a column never goes below
/// `TABLE_MIN_COL` (a one-letter-per-row column is worse than an overflowing
/// one) and never below its own natural width (shrinking a column that already
/// fits buys nothing). Columns pinned at a floor drop out and the rest re-share
/// what is left, repeating until nothing more is pinned — at most one pass per
/// column. If even the floors don't fit, the table overflows.
pub(crate) fn fit_columns(natural: &[Pixels], avail: Pixels) -> Vec<Pixels> {
    let nat: Vec<f32> = natural.iter().map(|w| f32::from(*w)).collect();
    let avail = f32::from(avail);
    if nat.is_empty() || nat.iter().sum::<f32>() <= avail {
        return natural.to_vec();
    }
    // Per-column floor: the global minimum, but never more than the column
    // actually wants.
    let floor: Vec<f32> = nat.iter().map(|w| w.min(TABLE_MIN_COL)).collect();
    let mut out = nat.clone();
    let mut pinned = vec![false; nat.len()];
    loop {
        let taken: f32 = (0..nat.len()).filter(|&i| pinned[i]).map(|i| out[i]).sum();
        let budget = avail - taken;
        let flex: Vec<usize> = (0..nat.len()).filter(|&i| !pinned[i]).collect();
        if flex.is_empty() {
            break;
        }
        let flex_total: f32 = flex.iter().map(|&i| nat[i]).sum();
        let mut changed = false;
        for &i in &flex {
            let share = if flex_total > 0. {
                budget * nat[i] / flex_total
            } else {
                budget / flex.len() as f32
            };
            if share < floor[i] {
                out[i] = floor[i];
                pinned[i] = true;
                changed = true;
            } else {
                out[i] = share;
            }
        }
        if !changed {
            break;
        }
    }
    out.into_iter().map(px).collect()
}

/// Column widths (padding included) for a table whose cells may hold images.
///
/// `text[c]` is the natural width of column `c`'s text and `image[c]` that of
/// its widest image (zero if it has none). A narrowed image is still a whole
/// picture, while narrowed text wraps — so images give way first: the text is
/// fitted as by [`fit_columns`] (an image column holding on to at least
/// `TABLE_MIN_COL` of picture), and only the room left after that goes to the
/// images, shared in proportion to how much more each column wants.
pub(crate) fn table_columns(text: &[Pixels], image: &[Pixels], avail: Pixels) -> Vec<Pixels> {
    let want: Vec<f32> = text.iter().zip(image).map(|(&t, &i)| f32::from(t.max(i))).collect();
    if want.iter().sum::<f32>() <= f32::from(avail) {
        return want.into_iter().map(px).collect();
    }
    let need: Vec<Pixels> = text
        .iter()
        .zip(image)
        .map(|(&t, &i)| t.max(i.min(px(TABLE_MIN_COL))))
        .collect();
    let base: Vec<f32> = fit_columns(&need, avail).into_iter().map(f32::from).collect();
    let spare = f32::from(avail) - base.iter().sum::<f32>();
    let excess: Vec<f32> = want.iter().zip(&base).map(|(w, b)| (w - b).max(0.)).collect();
    let total: f32 = excess.iter().sum();
    if spare <= 0. || total <= 0. {
        return base.into_iter().map(px).collect();
    }
    // `total > spare` (else everything would have fit), so no column is given
    // more than it wants.
    base.iter()
        .zip(&excess)
        .map(|(b, e)| px(b + spare * e / total))
        .collect()
}

/// Per-column alignment from the separator row (`:--` left, `--:` right, `:-:`).
pub(crate) fn table_aligns(sep: &str) -> Vec<TableAlign> {
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
pub(crate) enum TableOp {
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
pub(crate) fn edit_table(text: &str, line_idx: usize, col: usize, op: TableOp) -> Option<(String, usize, usize)> {
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

/// The spans/markers of the enclosing line that fall inside a cell, clipped to
/// it. Table syntax is not enabled in the document parse, so a row line is
/// parsed as ordinary text and its inline spans already cover the cells — which
/// means a cell can reuse the memoized document parse instead of re-parsing.
/// Everything stays in document coordinates.
pub(crate) fn cell_spans<'a>(
    line_spans: &'a [Span],
    line_markers: &'a [(usize, usize)],
    cell: Range<usize>,
) -> (Vec<Span>, Vec<(usize, usize)>) {
    let spans = line_spans
        .iter()
        .filter(|s| s.start < cell.end && s.end > cell.start)
        .map(|s| Span {
            start: s.start.max(cell.start),
            end: s.end.min(cell.end),
            attr: s.attr,
        })
        .collect();
    let markers = line_markers
        .iter()
        .copied()
        .filter(|&(a, b)| b > cell.start && a < cell.end)
        .map(|(a, b)| (a.max(cell.start), b.min(cell.end)))
        .collect();
    (spans, markers)
}

/// Build one table cell for rendering: its display string, styled runs, and the
/// source↔display segment map (for caret/click mapping). Inline markdown is
/// styled like body text, and — since the caret can sit inside a cell — markers
/// reveal per-span exactly as on a normal line. `cell_doc_start` is the document
/// offset of the cell's content; `spans`/`markers` are in document coordinates
/// (see [`cell_spans`]), so `cursor`/`selection` line up with them directly.
///
/// `breaks` are the row's `<br>`s (see [`cell_breaks`]); those in this cell
/// become line breaks. Unlike a marker, a break stays a break with the caret
/// beside it — that is where the caret sits after Enter — and shows raw only
/// with the caret inside the tag itself.
pub(crate) fn build_cell(
    cell: &str,
    cell_doc_start: usize,
    spans: &[Span],
    markers: &[(usize, usize)],
    breaks: &[Range<usize>],
    base_font: &Font,
    weight: FontWeight,
    cursor: usize,
    sel: Option<(usize, usize)>,
) -> (String, Vec<TextRun>, Vec<Seg>) {
    let cds = cell_doc_start;
    let mut ops = line_ops(cell, cds, markers, spans, cursor, sel);
    let mut dim = dim_ranges(cell, cds, markers, spans, cursor, sel);
    for r in breaks.iter().filter(|r| r.start >= cds && r.end <= cds + cell.len()) {
        let local = r.start - cds..r.end - cds;
        if r.start < cursor && cursor < r.end {
            dim.push((local.start, local.end)); // being edited: raw, grayed like markup
        } else if !ops.iter().any(|o| local.start < o.src.end && o.src.start < local.end) {
            ops.push(Op { src: local, repl: Repl::Break });
        }
    }
    ops.sort_by_key(|o| o.src.start);
    let (display, runs, segs, _checks) =
        build_line(cell, cds, &ops, spans, base_font, &dim, weight, false);
    (display, runs, segs)
}
