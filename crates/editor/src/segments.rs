//! The source<->display transform: concealment, substitution, styled runs.

use std::ops::Range;

use gpui::{
    px, rgb, rgba, Font, FontStyle, FontWeight, Hsla, StrikethroughStyle, TextRun, UnderlineStyle,
};

use crate::*;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct StyleKey {
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
pub(crate) struct Seg {
    pub(crate) src: Range<usize>,  // line-local source bytes
    pub(crate) disp: Range<usize>, // display bytes
    pub(crate) sub: bool,          // non-1:1 (hidden or substituted)
}

/// One line-local edit to the display: hide the source range, replace it with a
/// bullet glyph, reserve a slot for a painted checkbox, or break the line.
pub(crate) enum Repl {
    Hide,
    Bullet,
    Checkbox(bool), // checked?
    /// A table cell's `<br>`: a `\n` in the display, where the cell starts a
    /// new line (see `split_cell_lines`).
    Break,
}

pub(crate) struct Op {
    pub(crate) src: Range<usize>, // line-local
    pub(crate) repl: Repl,
}

/// A painted checkbox: where to draw it (display range on row 0) and the
/// document offset of the toggle char, for click-to-toggle.
#[derive(Clone)]
pub(crate) struct CheckHit {
    pub(crate) inner_abs: usize,   // document offset of the char between the brackets
    pub(crate) disp: Range<usize>, // display range of the reserved box slot
    pub(crate) checked: bool,
}

// The bullet dot and the reserved checkbox slot (an em-space wide enough for the
// painted box, plus a trailing gap before the text).
const BULLET: &str = "\u{2022}";
const CHECKBOX_SLOT: &str = "\u{2003} "; // EM SPACE + space

pub(crate) fn style_at(pos: usize, spans: &[Span]) -> StyleKey {
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

pub(crate) fn text_run(key: StyleKey, len: usize, base_font: &Font, base_weight: FontWeight) -> TextRun {
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

/// Whether an inline marker at document range `[ms, me)` should be revealed
/// (shown raw) — the Obsidian behavior of only showing markup for the element
/// the caret or selection is actually touching. The marker reveals when the
/// caret sits anywhere within its smallest enclosing inline span, or when a
/// selection overlaps that span (starts inside, ends inside, or wraps it).
pub(crate) fn marker_revealed(
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
pub(crate) fn dim_ranges(
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

/// The display transforms for a line. A heading's `#` reveals when the caret is
/// anywhere on the line; a bullet or checkbox reveals only when the caret is
/// next to it (within its `- ` / `- [ ] ` prefix). Inline markers (`**`/`*`/`` ` ``/link brackets)
/// reveal per-span: each shows raw only when the caret or selection touches its
/// own element (see [`marker_revealed`]) — otherwise it stays concealed even
/// while other parts of the same line are being edited.
pub(crate) fn line_ops(
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
    } else if let Some(info) = parse_list_line(line) {
        // Plain bullet: swap the marker char for a dot. Like a checkbox, the raw
        // marker shows only while the caret is next to it, not merely on the line.
        let near = cursor_col.is_some_and(|c| c >= info.marker.start && c < info.content);
        if !near {
            ops.push(Op { src: info.marker, repl: Repl::Bullet });
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

/// Build the display string, its text runs, the source↔display segment map, and
/// any clickable checkbox glyphs for one line, applying the given `ops`.
pub(crate) fn build_line(
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
                Repl::Break => {
                    display.push('\n');
                    runs.push(copy_run(StyleKey::default(), 1, base_font, base_weight, checked, false));
                    dpos += 1;
                }
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

/// Build a fenced-code-block content line: the source rendered verbatim (no
/// concealment, identity source↔display map) in the mono font, colored by the
/// pre-computed syntax-highlight runs. Any tail not covered by `hl` falls back
/// to the default text color.
pub(crate) fn build_code_line(
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

/// Map a source (line-local) byte offset to its display byte offset via segs.
pub(crate) fn src_to_disp(segs: &[Seg], src: usize) -> usize {
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
pub(crate) fn disp_to_src(segs: &[Seg], disp: usize) -> usize {
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
