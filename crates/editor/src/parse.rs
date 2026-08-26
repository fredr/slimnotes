//! Markdown parsing, styling classification and syntax highlighting.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

use gpui::{px, FontWeight, Hsla, Pixels, Rgba, SharedString};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::*;

/// Where a clicked link points.
#[derive(Clone)]
pub(crate) enum LinkTarget {
    Url(SharedString),  // external `[text](url)`
    Note(SharedString), // internal `[[note]]`
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Attr {
    Bold,
    Italic,
    Strike,
    Code,
    Link,
    Highlight,
    Tag,
}

#[derive(Clone)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) attr: Attr,
}

pub(crate) struct Checkbox {
    pub(crate) bracket: Range<usize>, // line-local `[ ]` range
    pub(crate) checked: bool,
}

pub(crate) struct ListInfo {
    pub(crate) marker: Range<usize>, // line-local list-marker char range (`-`/`*`/`+`)
    pub(crate) content: usize,       // first byte after the marker's whitespace
    pub(crate) checkbox: Option<Checkbox>,
}

/// One open inline element while walking the parser.
struct Frame {
    attr: Attr,
    cstart: usize,             // container start (incl. markers)
    istart: Option<usize>,     // inner content start
    iend: usize,               // inner content end
    url: Option<SharedString>, // set for links
}

pub(crate) type Links = Vec<(Range<usize>, LinkTarget)>;

pub(crate) fn parse(text: &str) -> (Vec<Span>, Vec<(usize, usize)>, Links) {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    let mut spans = Vec::new();
    let mut markers = Vec::new();
    let mut links: Links = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    // Byte ranges pulldown reports as code or raw HTML. The hand scanners
    // (wikilinks/highlights/tags) skip matches inside these so `==`, `#tag`, and
    // `[[` inside inline code or a code block aren't concealed/styled.
    let mut code_ranges: Vec<Range<usize>> = Vec::new();

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
                code_ranges.push(range.clone());
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
            // Fenced/indented code blocks and raw HTML: opaque to the scanners.
            Event::Start(Tag::CodeBlock(_)) | Event::Html(_) | Event::InlineHtml(_) => {
                code_ranges.push(range.clone());
            }
            _ => {}
        }
    }

    scan_wikilinks(text, &code_ranges, &mut spans, &mut markers, &mut links);
    scan_highlights(text, &code_ranges, &mut spans, &mut markers);
    scan_tags(text, &code_ranges, &mut spans);
    (spans, markers, links)
}

/// Whether byte `pos` falls inside any code/HTML range (so scanners ignore it).
fn in_code(pos: usize, code_ranges: &[Range<usize>]) -> bool {
    code_ranges.iter().any(|r| r.start <= pos && pos < r.end)
}

/// Whether the marker starting at `pos` is backslash-escaped (an odd run of
/// preceding backslashes).
fn escaped(text: &str, pos: usize) -> bool {
    text.as_bytes()[..pos].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

/// Obsidian `==highlight==`: conceal the `==` and give the text a highlight bg.
/// Uses CommonMark-ish flanking (no whitespace just inside the delimiters) so
/// prose like `2 == 2 and 3 == 3` isn't swallowed, and skips code/escaped `==`.
fn scan_highlights(
    text: &str,
    code_ranges: &[Range<usize>],
    spans: &mut Vec<Span>,
    markers: &mut Vec<(usize, usize)>,
) {
    let mut i = 0;
    while let Some(rel) = text[i..].find("==") {
        let s = i + rel;
        if in_code(s, code_ranges) || escaped(text, s) {
            i = s + 2;
            continue;
        }
        let Some(rel_end) = text[s + 2..].find("==") else { break };
        let e = s + 2 + rel_end + 2;
        let inner = &text[s + 2..e - 2];
        // Left/right flanking: no space right after the opening `==` nor right
        // before the closing `==`; and no newline inside.
        let ok = !inner.is_empty()
            && !inner.starts_with(char::is_whitespace)
            && !inner.ends_with(char::is_whitespace)
            && !inner.contains('\n')
            && !in_code(e - 2, code_ranges);
        if !ok {
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
fn scan_tags(text: &str, code_ranges: &[Range<usize>], spans: &mut Vec<Span>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || b[i - 1].is_ascii_whitespace() || b[i - 1] == b'(';
        if b[i] == b'#'
            && boundary
            && !in_code(i, code_ranges)
            && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
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
fn scan_wikilinks(
    text: &str,
    code_ranges: &[Range<usize>],
    spans: &mut Vec<Span>,
    markers: &mut Vec<(usize, usize)>,
    links: &mut Links,
) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find("[[") {
        let s = i + rel;
        if in_code(s, code_ranges) || escaped(text, s) {
            i = s + 2;
            continue;
        }
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

pub(crate) fn heading_style(line: &str) -> (Pixels, FontWeight) {
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

/// A blockquote prefix: leading `>`s (nested), returning the nesting level and
/// the byte length of the whole `> `/`> > ` prefix.
pub(crate) fn quote_prefix(line: &str) -> Option<(u8, usize)> {
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
pub(crate) fn is_hr(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.bytes().all(|b| b == b'-')
            || t.bytes().all(|b| b == b'*')
            || t.bytes().all(|b| b == b'_'))
}

/// Length of an ordered-list prefix (`12. ` or `12) `), for hanging indent.
pub(crate) fn ordered_prefix_len(line: &str) -> Option<usize> {
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

pub(crate) fn heading_prefix_len(line: &str) -> Option<usize> {
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

/// Detect a bullet/task list line and report the marker + optional checkbox
/// ranges (all line-local byte offsets).
pub(crate) fn parse_list_line(line: &str) -> Option<ListInfo> {
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
    Some(ListInfo {
        marker: indent..after_marker,
        content: i,
        checkbox: task_box(&line[i..]).map(|checked| Checkbox {
            bracket: i..i + 3,
            checked,
        }),
    })
}

/// A `[ ]` / `[x]` task box at the very start of `rest` (a list item's content),
/// followed by a space: reports whether it is checked. Compares raw bytes so a
/// multi-byte char between the brackets can't split a slice mid-character.
fn task_box(rest: &str) -> Option<bool> {
    let b = rest.as_bytes();
    (b.len() >= 4 && b[0] == b'[' && b[2] == b']' && b[3] == b' ').then(|| b[1] != b' ')
}

/// The edit that pressing Enter with the caret at byte `cursor` should make:
/// the byte range to replace and the text to put there. On a list item this
/// continues the list, and on an item that was left empty it clears the line —
/// ending the list with the same key that extends it. `in_code` suppresses both
/// (inside a fenced block a leading `- ` is code, not a list).
///
/// Kept here, and pure, so the whole behaviour is testable without a window;
/// see [`MarkdownEditor::insert_newline`] for the caller.
pub(crate) fn newline_edit(text: &str, cursor: usize, in_code: bool) -> (Range<usize>, String) {
    let plain = || (cursor..cursor, "\n".to_string());
    if in_code {
        return plain();
    }
    let (line_idx, col) = offset_to_line_col(text, cursor);
    let line = line_str(text, line_idx);
    // No marker, or the caret is still inside one: nothing to carry forward.
    let Some((prefix_len, next)) = list_prefix(line).filter(|(len, _)| col >= *len) else {
        return plain();
    };
    if line[prefix_len..].trim().is_empty() {
        // Clear the whole line rather than just the marker: what would be left
        // of an empty item is invisible trailing whitespace.
        let start = line_start(text, line_idx);
        return (start..start + line.len(), String::new());
    }
    (cursor..cursor, format!("\n{next}"))
}

/// A list item's marker: the byte length of the prefix that precedes its
/// content, and the marker the *next* item should carry — task boxes start out
/// unchecked and ordered numbers advance, everything else repeats verbatim.
fn list_prefix(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start().len();
    let (content, mut next) = if let Some(info) = parse_list_line(line) {
        // The spaces after `-`/`*`/`+` are part of the prefix, and are repeated
        // so an aligned list stays aligned.
        (info.content, line[..info.content].to_string())
    } else if let Some(end) = ordered_prefix_len(line) {
        // `ordered_prefix_len` counts the digits plus a two-byte `. ` / `) `.
        let digits = &line[indent..end - 2];
        let delim = &line[end - 2..end - 1];
        let n = match digits.parse::<u64>() {
            Ok(n) => n.saturating_add(1).to_string(),
            // Too many digits to be a real list number; keep it as written
            // rather than renumbering from nowhere.
            Err(_) => digits.to_string(),
        };
        (end, format!("{}{n}{delim} ", &line[..indent]))
    } else {
        return None;
    };

    match task_box(&line[content..]) {
        Some(_) => {
            next.push_str("[ ] ");
            Some((content + 4, next))
        }
        None => Some((content, next)),
    }
}

/// Decide how to render a checkbox line given the caret's line-local column
/// (`None` if the caret is on another line). Returns the `- [ ] ` prefix range,
/// whether the box is checked, and whether the glyph should be shown (vs raw).
/// Like Obsidian, the raw markdown is revealed only when the caret is *next to*
/// the checkbox — not merely somewhere on the line.
pub(crate) fn checkbox_render(line: &str, cursor_col: Option<usize>) -> Option<(Range<usize>, bool, bool)> {
    let info = parse_list_line(line)?;
    let cb = info.checkbox?;
    let prefix_end = cb.bracket.end + 1; // through the space after `]`
    let near = cursor_col.is_some_and(|c| c >= info.marker.start && c < prefix_end);
    Some((info.marker.start..prefix_end, cb.checked, !near))
}

/// Per-line code-fence classification.
#[derive(Clone, Copy)]
pub(crate) struct CodeLine {
    pub(crate) in_block: bool, // inside a fenced block (fence lines included) -> code panel
    pub(crate) is_fence: bool, // the line is a ``` fence
    pub(crate) block: usize,   // 1-based block id (0 when not in a block)
}

/// Whether line `li`'s ``` fence should be concealed: only when it's a fence and
/// the caret is *not* inside that same block (Obsidian reveals fences on edit).
pub(crate) fn fence_hidden(code: &[CodeLine], cursor_line: usize, li: usize) -> bool {
    let cursor_block = if code[cursor_line].in_block {
        code[cursor_line].block
    } else {
        0
    };
    code[li].is_fence && code[li].block != cursor_block
}

pub(crate) fn classify_code(text: &str) -> Vec<CodeLine> {
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

/// The line index of the closing `---` of YAML frontmatter (a `---` fence on the
/// very first line), if present. Those lines render as a de-emphasized panel.
pub(crate) fn frontmatter_end(text: &str) -> Option<usize> {
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
pub(crate) fn highlight_code(text: &str, code: &[CodeLine]) -> HashMap<usize, Vec<(usize, Hsla)>> {
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

/// Memoized parse of the whole buffer (see [`MarkdownEditor::ensure_parsed`]).
/// `line_spans`/`line_markers` bucket the global spans/markers by the source line
/// they touch, so per-line building doesn't scan every span.
pub(crate) struct ParsedDoc {
    pub(crate) links: Links,
    pub(crate) code: Vec<CodeLine>,
    pub(crate) fm_end: Option<usize>,
    pub(crate) line_spans: Vec<Vec<Span>>,
    pub(crate) line_markers: Vec<Vec<(usize, usize)>>,
    /// Syntax-highlight colors for fenced code-block lines (line index -> run
    /// list of `(byte_len, color)`). Computed once per edit, not per frame.
    pub(crate) code_hl: HashMap<usize, Vec<(usize, Hsla)>>,
}
