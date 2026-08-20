//! Everything tables: parsing, rendering geometry, structural edits and the
//! cell context menu.

use std::ops::Range;

use gpui::{px, Font, FontWeight, Pixels, Point, TextRun};

use crate::*;

pub(crate) const TABLE_PAD_X: f32 = 10.0; // horizontal cell padding (px)
pub(crate) const TABLE_PAD_Y: f32 = 5.0; // vertical cell padding (px)
pub(crate) const TABLE_FONT: f32 = 16.0; // table cell font size (px)
pub(crate) const TABLE_BORDER: u32 = 0x3a3a3a; // cell border color

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

/// A rendered table cell: its shaped text plus column geometry (x/width are
/// relative to the table's left edge) and the source mapping needed to place the
/// caret / hit-test clicks inside it for in-cell editing.
#[derive(Clone)]
pub(crate) struct TableCell {
    pub(crate) shaped: gpui::ShapedLine,
    pub(crate) segs: Vec<Seg>,   // cell-local source <-> display transform
    pub(crate) src_start: usize, // document offset of the cell's content start
    pub(crate) src_end: usize,   // document offset of the cell's content end
    pub(crate) x: Pixels,
    pub(crate) width: Pixels,
    pub(crate) align: TableAlign,
}

impl TableCell {
    /// Horizontal offset of the shaped text within the cell box (alignment pad).
    pub(crate) fn text_x(&self) -> Pixels {
        let tw = self.shaped.width;
        match self.align {
            TableAlign::Left => px(TABLE_PAD_X),
            TableAlign::Right => (self.width - px(TABLE_PAD_X) - tw).max(px(TABLE_PAD_X)),
            TableAlign::Center => ((self.width - tw) / 2.).max(px(TABLE_PAD_X)),
        }
    }
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
pub(crate) fn table_cell_spans(line: &str) -> Vec<Range<usize>> {
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
pub(crate) fn build_cell(
    cell: &str,
    cell_doc_start: usize,
    spans: &[Span],
    markers: &[(usize, usize)],
    base_font: &Font,
    weight: FontWeight,
    cursor: usize,
    sel: Option<(usize, usize)>,
) -> (String, Vec<TextRun>, Vec<Seg>) {
    let cds = cell_doc_start;
    let ops = line_ops(cell, cds, markers, spans, cursor, sel);
    let dim = dim_ranges(cell, cds, markers, spans, cursor, sel);
    let (display, runs, segs, _checks) =
        build_line(cell, cds, &ops, spans, base_font, &dim, weight, false);
    (display, runs, segs)
}
